use cudarc::{
    driver::{CudaDevice, CudaSlice, LaunchAsync, LaunchConfig},
    nvrtc::compile_ptx_with_opts,
};
use itertools::Itertools;
use petgraph::visit::EdgeRef;

use crate::{op::Operator, prelude::*};

// Ops and optimizers specific to CUDA execution

pub type CudaOptimizer = (CudaPrimitiveOptimizer,);

impl Data for CudaSlice<f32> {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Convert all primitive ops to cuda primitive ops, and insert copy to and from device ops
#[derive(Debug, Default)]
pub struct CudaPrimitiveOptimizer;

impl GraphOptimizer for CudaPrimitiveOptimizer {
    fn optimize(&self, graph: &mut Graph) {
        // Go through the graph and insert copy ops
        // Copy to device
        for (input_node, input_shape) in graph
            .graph
            .node_indices()
            .filter(|n| graph.graph.node_weight(*n).unwrap().0.name() == "Input")
            .map(|n| (n, graph.graph.node_weight(n).unwrap().1[0].clone()))
            .collect_vec()
        {
            // Create copy node
            let copy_node = graph
                .add_op(CudaCopyToDevice)
                .input(input_node, input_shape)
                .finish();

            // Switch outgoing edges from input to copy_node
            for (edge_id, weight, dest) in graph
                .graph
                .edges_directed(input_node, petgraph::Direction::Outgoing)
                .map(|e| (e.id(), *e.weight(), e.target()))
                .filter(|(_, _, trg)| *trg != copy_node)
                .collect_vec()
            {
                graph.graph.add_edge(copy_node, dest, weight);
                graph.graph.remove_edge(edge_id);
            }

            // This isn't a great way to do this since we don't actually want to save the output of the copy node, just mark it to get a copy back node right below
            if graph.no_delete.contains(&input_node) {
                graph.no_delete.insert(copy_node);
            }
        }

        // Copy from device
        for (output_node, output_shape) in graph
            .no_delete
            .iter()
            .filter(|n| graph.graph.node_weight(**n).unwrap().0.name() != "Input")
            .map(|n| (*n, graph.graph.node_weight(*n).unwrap().1[0].clone()))
            .collect_vec()
        {
            // Create copy node
            let copy_node = graph
                .add_op(CudaCopyFromDevice)
                .input(output_node, output_shape)
                .finish();

            Graph::move_references(
                &mut graph.id_remap,
                &mut graph.no_delete,
                output_node,
                copy_node,
            );
        }

        // Swap primitive ops
        for (id, name) in graph
            .graph
            .node_indices()
            .map(|n| (n, graph.graph.node_weight(n).unwrap().0.name()))
            .collect_vec()
        {
            match name {
                "Log2" => graph.graph.node_weight_mut(id).unwrap().0 = Box::new(CudaLog2),
                "Exp2" => graph.graph.node_weight_mut(id).unwrap().0 = Box::new(CudaExp2),
                "Sin" => graph.graph.node_weight_mut(id).unwrap().0 = Box::new(CudaSin),
                "Sqrt" => graph.graph.node_weight_mut(id).unwrap().0 = Box::new(CudaSqrt),
                "Recip" => graph.graph.node_weight_mut(id).unwrap().0 = Box::new(CudaRecip),
                _ => {}
            };
        }
    }
}

/// Copy a tensor to the GPU
#[derive(Debug)]
pub struct CudaCopyToDevice;

impl Operator for CudaCopyToDevice {
    fn name(&self) -> &'static str {
        "CudaCopyToDevice"
    }

    fn process(&self, inp: Vec<&Tensor>) -> Tensor {
        let dev = CudaDevice::new(0).unwrap();
        let cpu_data = inp[0].data.as_any().downcast_ref::<Vec<f32>>().unwrap();
        let mut a: CudaSlice<f32> = dev.alloc_zeros::<f32>(cpu_data.len()).unwrap();
        dev.htod_sync_copy_into(cpu_data, &mut a).unwrap();
        Tensor {
            data: Box::new(a),
            shape: inp[0].shape.clone(),
        }
    }
}

/// Copy a tensor from the GPU
#[derive(Debug)]
pub struct CudaCopyFromDevice;

impl Operator for CudaCopyFromDevice {
    fn name(&self) -> &'static str {
        "CudaCopyFromDevice"
    }

    fn process(&self, inp: Vec<&Tensor>) -> Tensor {
        let dev = CudaDevice::new(0).unwrap();
        let cuda_data = inp[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let a = dev.dtoh_sync_copy(cuda_data).unwrap();
        Tensor {
            data: Box::new(a),
            shape: inp[0].shape.clone(),
        }
    }
}

// Unary Op (A -> A)

#[derive(Debug, Clone)]
pub struct CudaLog2;
impl Operator for CudaLog2 {
    fn name(&self) -> &'static str {
        "CudaLog2"
    }
    fn process(&self, tensors: Vec<&Tensor>) -> Tensor {
        let inp = tensors[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let inp_size: usize = tensors[0].shape.shape().iter().product();
        let ptx = compile_ptx_with_opts(
            "
extern \"C\" __global__ void log2_kernel(float *out, const float *inp, int numel) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {
        out[i] = log2(inp[i]);
    }
}",
            Default::default(),
        )
        .unwrap();
        let dev = CudaDevice::new(0).unwrap();
        dev.load_ptx(ptx, "log2", &["log2_kernel"]).unwrap();
        let f = dev.get_func("log2", "log2_kernel").unwrap();

        let mut out = unsafe { dev.alloc::<f32>(inp_size) }.unwrap();
        let cfg = LaunchConfig::for_num_elems(inp_size as u32);
        unsafe { f.launch(cfg, (&mut out, inp, inp_size as i32)) }.unwrap();

        Tensor {
            data: Box::new(out),
            shape: tensors[0].shape.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CudaExp2;
impl Operator for CudaExp2 {
    fn name(&self) -> &'static str {
        "CudaExp2"
    }
    fn process(&self, tensors: Vec<&Tensor>) -> Tensor {
        let inp = tensors[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let inp_size: usize = tensors[0].shape.shape().iter().product();
        let ptx = compile_ptx_with_opts(
            "
extern \"C\" __global__ void exp2_kernel(float *out, const float *inp, int numel) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {
        out[i] = exp2(inp[i]);
    }
}",
            Default::default(),
        )
        .unwrap();
        let dev = CudaDevice::new(0).unwrap();
        dev.load_ptx(ptx, "exp2", &["exp2_kernel"]).unwrap();
        let f = dev.get_func("exp2", "exp2_kernel").unwrap();

        let mut out = unsafe { dev.alloc::<f32>(inp_size) }.unwrap();
        let cfg = LaunchConfig::for_num_elems(inp_size as u32);
        unsafe { f.launch(cfg, (&mut out, inp, inp_size as i32)) }.unwrap();

        Tensor {
            data: Box::new(out),
            shape: tensors[0].shape.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CudaSin;
impl Operator for CudaSin {
    fn name(&self) -> &'static str {
        "CudaSin"
    }
    fn process(&self, tensors: Vec<&Tensor>) -> Tensor {
        let inp = tensors[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let inp_size: usize = tensors[0].shape.shape().iter().product();
        let ptx = compile_ptx_with_opts(
            "
extern \"C\" __global__ void sin_kernel(float *out, const float *inp, int numel) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {
        out[i] = sin(inp[i]);
    }
}",
            Default::default(),
        )
        .unwrap();
        let dev = CudaDevice::new(0).unwrap();
        dev.load_ptx(ptx, "sin", &["sin_kernel"]).unwrap();
        let f = dev.get_func("sin", "sin_kernel").unwrap();

        let mut out = unsafe { dev.alloc::<f32>(inp_size) }.unwrap();
        let cfg = LaunchConfig::for_num_elems(inp_size as u32);
        unsafe { f.launch(cfg, (&mut out, inp, inp_size as i32)) }.unwrap();

        Tensor {
            data: Box::new(out),
            shape: tensors[0].shape.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CudaSqrt;
impl Operator for CudaSqrt {
    fn name(&self) -> &'static str {
        "CudaSqrt"
    }
    fn process(&self, tensors: Vec<&Tensor>) -> Tensor {
        let inp = tensors[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let inp_size: usize = tensors[0].shape.shape().iter().product();
        let ptx = compile_ptx_with_opts(
            "
extern \"C\" __global__ void sqrt_kernel(float *out, const float *inp, int numel) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {
        out[i] = sqrt(inp[i]);
    }
}",
            Default::default(),
        )
        .unwrap();
        let dev = CudaDevice::new(0).unwrap();
        dev.load_ptx(ptx, "sqrt", &["sqrt_kernel"]).unwrap();
        let f = dev.get_func("sqrt", "sqrt_kernel").unwrap();

        let mut out = unsafe { dev.alloc::<f32>(inp_size) }.unwrap();
        let cfg = LaunchConfig::for_num_elems(inp_size as u32);
        unsafe { f.launch(cfg, (&mut out, inp, inp_size as i32)) }.unwrap();

        Tensor {
            data: Box::new(out),
            shape: tensors[0].shape.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CudaRecip;
impl Operator for CudaRecip {
    fn name(&self) -> &'static str {
        "CudaRecip"
    }
    fn process(&self, tensors: Vec<&Tensor>) -> Tensor {
        let inp = tensors[0]
            .data
            .as_any()
            .downcast_ref::<CudaSlice<f32>>()
            .unwrap();
        let inp_size: usize = tensors[0].shape.shape().iter().product();
        let ptx = compile_ptx_with_opts(
            "
extern \"C\" __global__ void recip_kernel(float *out, const float *inp, int numel) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {
        out[i] = 1.0 / inp[i];
    }
}",
            Default::default(),
        )
        .unwrap();
        let dev = CudaDevice::new(0).unwrap();
        dev.load_ptx(ptx, "recip", &["recip_kernel"]).unwrap();
        let f = dev.get_func("recip", "recip_kernel").unwrap();

        let mut out = unsafe { dev.alloc::<f32>(inp_size) }.unwrap();
        let cfg = LaunchConfig::for_num_elems(inp_size as u32);
        unsafe { f.launch(cfg, (&mut out, inp, inp_size as i32)) }.unwrap();

        Tensor {
            data: Box::new(out),
            shape: tensors[0].shape.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use dfdx::prelude::*;

    use super::CudaOptimizer;
    use crate::{prelude::*, tests::assert_close_data};

    #[test]
    fn test_log2() {
        // We can't use dfdx because it doesn't implement this op
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.log_2();
        b.mark();

        cx.optimize(CudaOptimizer::default());
        cx.execute();

        assert_close_data(
            &b.retrieve().unwrap().real_data().unwrap(),
            &vec![1., 2., 3.]
                .into_iter()
                .map(|i: f32| i.log2())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_exp2() {
        // We can't use dfdx because it doesn't implement this op
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.exp_2();
        b.mark();

        cx.optimize(CudaOptimizer::default());
        cx.execute();

        assert_close_data(
            &b.retrieve().unwrap().real_data().unwrap(),
            &vec![1., 2., 3.]
                .into_iter()
                .map(|i: f32| i.exp2())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_log2exp2() {
        // We can't use dfdx because it doesn't implement this op
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.exp_2().log_2();
        b.mark();

        cx.optimize(<(GeneralOpt, CudaOptimizer)>::default());
        cx.execute();

        assert_close_data(&b.retrieve().unwrap().real_data().unwrap(), &[1., 2., 3.]);
    }

    #[test]
    fn test_recip() {
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.recip();
        b.mark();
        cx.optimize(CudaOptimizer::default());
        cx.execute();

        let d_dev = Cpu::default();
        let d_a = d_dev.tensor([1., 2., 3.]);
        let d_b = d_a.recip();

        assert_close_data(&b.retrieve().unwrap().real_data().unwrap(), &d_b.as_vec());
    }

    #[test]
    fn test_sin() {
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.sin();
        b.mark();
        cx.optimize(CudaOptimizer::default());
        cx.execute();

        let d_dev = Cpu::default();
        let d_a = d_dev.tensor([1., 2., 3.]);
        let d_b = d_a.sin();

        assert_close_data(&b.retrieve().unwrap().real_data().unwrap(), &d_b.as_vec());
    }

    #[test]
    fn test_sqrt() {
        let mut cx = Graph::new();
        let a = cx.new_tensor::<R1<3>>();
        a.set(vec![1., 2., 3.]);
        let b = a.sqrt();
        b.mark();
        cx.optimize(CudaOptimizer::default());
        cx.execute();

        let d_dev = Cpu::default();
        let d_a = d_dev.tensor([1., 2., 3.]);
        let d_b = d_a.sqrt();

        assert_close_data(&b.retrieve().unwrap().real_data().unwrap(), &d_b.as_vec());
    }
}
