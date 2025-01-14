mod binary;
mod elementwise_fusion;
mod matmul;
mod other;
mod prim;

#[cfg(test)]
mod tests;

use itertools::Itertools;
use luminal_cudarc::{
    driver::{CudaDevice, CudaFunction, CudaSlice, DeviceRepr},
    nvrtc::{compile_ptx_with_opts, CompileOptions},
};
use prim::CudaConstant;
use rustc_hash::FxHashMap;

use std::{collections::hash_map::DefaultHasher, ffi::c_void, fmt::Write, hash::Hasher, sync::Arc};

use luminal::{op::InputTensor, prelude::*};

use self::symbolic::{BigExpression, Term};

pub type CudaCompiler<T> = (
    prim::CudaPrimitiveCompiler<T>,
    binary::CudaSubtractionCompiler<T>,
    binary::CudaEqualCompiler<T>,
    other::ARangeCompiler<T>,
    binary::MetalGatherCompiler<T>,
    matmul::CudaMatMulCompiler<T>,
    prim::CopyCompiler<T>,
);

pub trait CudaFloat:
    std::fmt::Debug
    + Copy
    + luminal_cudarc::driver::DeviceRepr
    + std::marker::Unpin
    + luminal_cudarc::driver::ValidAsZeroBits
    + 'static
{
    fn to_f32(self) -> f32;
    fn from_f32(a: f32) -> Self;
    fn is_f32() -> bool;
    fn type_name() -> &'static str;
}

impl CudaFloat for f32 {
    fn from_f32(a: f32) -> Self {
        a
    }
    fn to_f32(self) -> f32 {
        self
    }
    fn is_f32() -> bool {
        true
    }
    fn type_name() -> &'static str {
        "float"
    }
}
#[derive(Debug)]
pub struct CudaData<T>(CudaSlice<T>);

impl<T: DeviceRepr> Clone for CudaData<T> {
    fn clone(&self) -> Self {
        Self(self.0.try_clone().unwrap())
    }
}

impl<T: CudaFloat> Data for CudaData<T> {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl CudaFloat for f16 {
    fn from_f32(a: f32) -> Self {
        f16::from_f32(a)
    }
    fn to_f32(self) -> f32 {
        self.to_f32()
    }
    fn is_f32() -> bool {
        false
    }
    fn type_name() -> &'static str {
        "__half"
    }
}

fn expr_to_cuda_string(expr: BigExpression) -> String {
    let mut symbols = vec![];
    for term in expr.terms {
        let new_symbol = match term {
            Term::Num(n) => n.to_string(),
            Term::Var(c) => {
                if c == 'z' {
                    "(int)idx".to_string()
                } else {
                    c.to_string()
                }
            }
            Term::Max => format!(
                "max((int){}, (int){})",
                symbols.pop().unwrap(),
                symbols.pop().unwrap()
            ),
            Term::Min => format!(
                "min((int){}, (int){})",
                symbols.pop().unwrap(),
                symbols.pop().unwrap()
            ),
            _ => format!(
                "({}{term:?}{})",
                symbols.pop().unwrap(),
                symbols.pop().unwrap()
            ),
        };
        symbols.push(new_symbol);
    }
    symbols.pop().unwrap()
}

fn get_idx_valid_exps(shape: ShapeTracker) -> (String, String) {
    (
        expr_to_cuda_string(shape.index_expression()),
        expr_to_cuda_string(shape.valid_expression()),
    )
}

fn render_dyn_dim_inputs(shapes: &[ShapeTracker]) -> (Vec<char>, String) {
    let symbols: Vec<char> = shapes
        .iter()
        .flat_map(|st| {
            st.shape()
                .into_iter()
                .chain(
                    st.padding
                        .into_iter()
                        .flat_map(|i| [i.0.into(), i.1.into()]),
                )
                .chain(st.slices.into_iter().flat_map(|i| [i.0.into(), i.1.into()]))
        })
        .flat_map(|d| d.to_symbols())
        .unique()
        .collect();
    (
        symbols.clone(),
        symbols.into_iter().fold(String::default(), |mut acc, c| {
            write!(&mut acc, ", const int {c}").unwrap();
            acc
        }),
    )
}

pub fn constant<T: CudaFloat>(num: f32) -> SelectGraph
where
    CudaData<T>: Data,
{
    let mut n = op::<CudaConstant<T>>();
    n.check(move |o, _| {
        if let Some(c) = o.as_any().downcast_ref::<CudaConstant<T>>() {
            if let luminal::op::ConstantValue::Float(f) = c.value {
                f == num
            } else {
                false
            }
        } else {
            false
        }
    });
    n
}

fn hash<T: std::hash::Hash>(obj: T) -> u64 {
    let mut hasher = DefaultHasher::new();
    obj.hash(&mut hasher);
    hasher.finish()
}

fn get_buffer_from_tensor<'a, T: 'static>(tensor: &'a InputTensor) -> &'a CudaSlice<T> {
    &tensor
        .borrowed()
        .data
        .as_any()
        .downcast_ref::<CudaData<T>>()
        .unwrap()
        .0
}

fn input_dyn_dims(
    params: &mut Vec<*mut c_void>,
    dyn_symbols: &[char],
    dyn_map: *const FxHashMap<char, usize>,
) {
    let dyn_map = unsafe { dyn_map.as_ref().unwrap() };
    for d in dyn_symbols {
        params.push(dyn_map[d].as_kernel_param());
    }
}

fn compile_and_load_kernel(mut code: String, device: &Arc<CudaDevice>) -> CudaFunction {
    let name = format!("kernel_{}", hash(&code));
    code = code.replace("kernel", &name);
    if !device.has_func(&name, &name) {
        device
            .load_ptx(
                compile_ptx_with_opts(
                    code,
                    CompileOptions {
                        arch: Some("sm_75"),
                        include_paths: vec!["/usr/local/cuda/include".to_string()],
                        ..Default::default()
                    },
                )
                .unwrap(),
                &name,
                &[name.clone().leak()],
            )
            .unwrap();
    }
    device.get_func(&name, &name).unwrap()
}
