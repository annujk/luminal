use std::{
    env,
    io::{self, Write},
    marker::PhantomData,
    time::Instant,
};

use clap::{Args, Parser};
use colored::Colorize;
use half::f16;
use rust_tokenizers::tokenizer::{SentencePieceBpeTokenizer, Tokenizer, TruncationStrategy};
mod loader;
mod model;

use luminal::{prelude::*, shape::symbolic::Expression};

use crate::model::KVCache;

#[cfg(feature = "metal")]
type DeviceCompiler = MetalCompiler<f16>;
#[cfg(feature = "cuda")]
type DeviceCompiler = CudaFp16Compiler;
#[cfg(all(not(feature = "cuda"), not(feature = "metal")))]
type DeviceCompiler = CPUCompiler;

// Command args parser
#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct CLIArgs {
    /// Number of tokens to generate
    #[clap(short = 't', long = "gen_tokens", default_value = "128")]
    gen_tokens: i32,

    /// Prompt for the model
    #[clap(short = 'p', long = "prompt", default_value = include_str!("prompts/shakespeare.txt"))]
    prompt: String,
}

fn main() {
    let cli_args = CLIArgs::parse();
    let prompt = cli_args.prompt.as_str();
    let tokens_to_generate = cli_args.gen_tokens;

    let tokenizer = SentencePieceBpeTokenizer::from_file(
        "./examples/mistral/setup/mistral-7b-hf/tokenizer.model",
        false,
    )
    .unwrap();

    print!("Defining Graphs");
    io::stdout().flush().unwrap();
    let now = Instant::now();
    let mut cx1 = Graph::new();
    let mut input = cx1.named_tensor::<(Const<1>, Dyn<'s'>)>("Input");
    let model = model::MistralLM::initialize(&mut cx1);
    let (logits, mut kv_cache) = model.forward((
        input,
        Option::<Vec<KVCache<Const<1>, Const<0>>>>::None,
        PhantomData::<Dyn<'s'>>,
    ));
    let mut logits = logits
        .slice((.., (Expression::from('s') - 1).., ..))
        .retrieve();
    kv_cache.keep();

    // Set up model loading
    loader::MetalFp16SafetensorsLoader::new(&[
        "./examples/mistral/setup/mistral-7b-hf/converted-model-00001-of-00003.safetensors",
        "./examples/mistral/setup/mistral-7b-hf/converted-model-00002-of-00003.safetensors",
        "./examples/mistral/setup/mistral-7b-hf/converted-model-00003-of-00003.safetensors",
    ])
    .load(&model, &mut cx1);

    // KV cache graph
    let mut cx2 = Graph::new();
    let mut single_input = cx2.named_tensor::<R2<1, 1>>("Input");
    let kv_model = model::MistralLM::initialize(&mut cx2);
    let mut cache_src: Vec<KVCache<Const<1>, Dyn<'p'>>> = (0..model::NUM_LAYERS)
        .map(|_| {
            (
                cx2.named_tensor("Key Cache"),
                cx2.named_tensor("Value Cache"),
            )
        })
        .collect();
    let (mut decode_logits, mut cache_dest) = kv_model.forward((
        single_input,
        Some(cache_src.clone()),
        PhantomData::<Dyn<'t'>>,
    ));
    decode_logits.retrieve();
    cache_dest.keep();
    println!("\t - {}ms", now.elapsed().as_millis());

    print!("Compiling Prompt Processing Graph");
    io::stdout().flush().unwrap();
    let now = Instant::now();
    cx1.compile(
        GenericCompiler::<DeviceCompiler>::default(),
        (&mut input, &mut logits, &mut kv_cache),
    );
    let model_weights = downstream(&state_set(&model), &cx1);
    cx1.no_delete.extend(model_weights.clone());
    println!("\t - {}ms", now.elapsed().as_millis());

    print!("Compiling Token Generation Graph");
    io::stdout().flush().unwrap();
    let now = Instant::now();
    // Compile second graph
    cx2.compile(
        GenericCompiler::<DeviceCompiler>::default(),
        (
            &mut single_input,
            &mut decode_logits,
            &mut cache_src,
            &mut cache_dest,
        ),
    );
    let kv_model_weights = downstream(&state_set(&kv_model), &cx2);
    cx2.no_delete.extend(kv_model_weights.clone());
    let cache_src_set = downstream(&cache_src, &cx2);
    let cache_dest_set = cache_dest.to_ids();
    delete_inputs(&kv_model_weights, &mut cx2);
    delete_inputs(&cache_src_set, &mut cx2);
    println!("\t - {}ms", now.elapsed().as_millis());

    // Initial forward pass to load weights
    print!("Loading model");
    io::stdout().flush().unwrap();
    let now = Instant::now();
    input.set_dyn(vec![1.], vec![1, 1]);
    cx1.execute();
    logits.drop();
    kv_cache.drop();
    println!("\t - {}ms", now.elapsed().as_millis());

    // Now that weights are loaded, delete the loading nodes so they don't run again
    delete_inputs(&model_weights, &mut cx1);

    // Run inference first pass
    let mut input_ids = encode(&tokenizer, prompt);

    input.set_dyn(
        input_ids.iter().map(|i| *i as f32).collect::<Vec<_>>(),
        vec![1, input_ids.len()],
    );
    print!("Processing Prompt");
    io::stdout().flush().unwrap();
    let now = Instant::now();
    cx1.execute();
    let elapsed_ms = now.elapsed().as_millis();
    let n_prompt_tokens = input_ids.len();
    let pp_speed = 1000.0 * (n_prompt_tokens as f64) / (elapsed_ms as f64);
    println!("\t - {}ms ({:.2} tok/s)", elapsed_ms, pp_speed);

    let output_id = sample_index(&logits.data());
    input_ids.push(output_id);

    // Decode token
    print!(
        "{}{}",
        prompt.white().bold(),
        decode(&tokenizer, &[output_id]).bright_green()
    );
    io::stdout().flush().unwrap();

    // Transfer weights and kv cache
    transfer_data(&model_weights, &mut cx1, &kv_model_weights, &mut cx2);
    transfer_data(&kv_cache, &mut cx1, &cache_src_set, &mut cx2);
    drop(cx1);

    // Decode loop
    let mut token_decode_times = vec![];
    for _ in 0..tokens_to_generate {
        single_input.set(vec![*input_ids.last().unwrap() as f32]);
        cx2.set_dyn_dim('p', input_ids.len() - 1);
        cx2.set_dyn_dim('t', input_ids.len());

        let now = Instant::now();
        cx2.execute();
        token_decode_times.push(now.elapsed().as_millis());

        // Sample tokens
        let output_id = sample_index(&decode_logits.data());
        decode_logits.drop();
        input_ids.push(output_id);
        print!("{}", decode(&tokenizer, &[output_id]).bright_green());
        io::stdout().flush().unwrap();

        // Swap caches
        transfer_data_same_graph(&cache_dest_set, &cache_src_set, &mut cx2);
    }
    println!(
        "\nAverage token generated in {}ms",
        token_decode_times.iter().sum::<u128>() / token_decode_times.len() as u128
    );
}

fn encode(tokenizer: &SentencePieceBpeTokenizer, text: &str) -> Vec<i64> {
    let mut vector = tokenizer
        .encode(text, None, text.len(), &TruncationStrategy::LongestFirst, 0)
        .token_ids;
    vector.insert(0, 1); // Start token
    vector
}

fn decode(tokenizer: &SentencePieceBpeTokenizer, token_ids: &[i64]) -> String {
    tokenizer
        .decode(token_ids, true, false)
        .replace("<0x0A>", "\n")
}

// Currently just an argmax, do actual sampling here
fn sample_index(dist: &[f32]) -> i64 {
    dist.iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap()
        .0 as i64
}
