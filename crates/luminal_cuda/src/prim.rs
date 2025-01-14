use crate::{compile_and_load_kernel, get_buffer_from_tensor, input_dyn_dims, CudaData, CudaFloat};

use super::{get_idx_valid_exps, render_dyn_dim_inputs};
use itertools::Itertools;
use rustc_hash::FxHashMap;

use std::{
    any::{Any, TypeId},
    fmt::Debug,
    marker::PhantomData,
    sync::Arc,
};

use luminal_cudarc::driver::{CudaDevice, CudaFunction, DeviceRepr, LaunchAsync, LaunchConfig};

use luminal::{
    op::{Function as LFunction, *},
    prelude::{petgraph::visit::EdgeRef, *},
};

/// Copy a tensor to the GPU
#[derive(Clone, LuminalEqFalse, LuminalPrint)]
pub struct CudaCopyToDevice<T>(Arc<CudaDevice>, PhantomData<T>);

impl<T> CudaCopyToDevice<T> {
    pub fn new(dev: Arc<CudaDevice>) -> Self {
        CudaCopyToDevice(dev, Default::default())
    }
}

impl<T: CudaFloat> Operator for CudaCopyToDevice<T> {
    fn process(&mut self, mut inp: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        if inp[0].0.borrowed().data.as_any().is::<CudaData<T>>() {
            // Already on device
            return vec![inp.pop().unwrap().0.cloned()];
        }
        let cpu_data = inp[0]
            .0
            .borrowed()
            .data
            .as_any()
            .downcast_ref::<Vec<f32>>()
            .unwrap();
        let vec = cpu_data
            .iter()
            .copied()
            .map(CudaFloat::from_f32)
            .collect::<Vec<_>>();
        let mut a = unsafe { self.0.alloc::<T>(vec.len()).unwrap() };
        self.0.htod_copy_into(vec, &mut a).unwrap();
        vec![Tensor::new(CudaData(a))]
    }
}

/// Copy a tensor from the GPU
#[derive(Clone, LuminalEqFalse, LuminalPrint)]
pub struct CudaCopyFromDevice<T>(Arc<CudaDevice>, PhantomData<T>);

impl<T> CudaCopyFromDevice<T> {
    pub fn new(dev: Arc<CudaDevice>) -> Self {
        CudaCopyFromDevice(dev, Default::default())
    }
}

impl<T: CudaFloat> Operator for CudaCopyFromDevice<T> {
    fn process(&mut self, mut inp: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        if inp[0].0.borrowed().data.as_any().is::<Vec<f32>>() {
            // Already off device
            return vec![inp.pop().unwrap().0.cloned()];
        }
        vec![Tensor::new(
            self.0
                .dtoh_sync_copy(get_buffer_from_tensor::<T>(&inp[0].0))
                .unwrap()
                .into_iter()
                .map(CudaFloat::to_f32)
                .collect::<Vec<_>>(),
        )]
    }
}

/// Constant value on device
#[derive(Clone, LuminalEqFalse)]
pub struct CudaConstant<T> {
    pub value: ConstantValue,
    device: Arc<CudaDevice>,
    dyn_map: *const FxHashMap<char, usize>,
    _phantom: PhantomData<T>,
}
impl<T> Debug for CudaConstant<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CudaConstant({:?})", self.value)
    }
}

impl<T> CudaConstant<T> {
    pub fn new(
        device: Arc<CudaDevice>,
        value: ConstantValue,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        Self {
            value,
            device,
            dyn_map,
            _phantom: Default::default(),
        }
    }
}

impl<T: CudaFloat> Operator for CudaConstant<T> {
    fn process(&mut self, _: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let mut a = unsafe { self.device.alloc::<T>(1).unwrap() };
        let value = match &self.value {
            ConstantValue::Expression(e) => {
                T::from_f32(e.exec(unsafe { self.dyn_map.as_ref().unwrap() }).unwrap() as f32)
            }
            ConstantValue::Float(f) => T::from_f32(*f),
        };
        self.device.htod_copy_into(vec![value], &mut a).unwrap();
        vec![Tensor::new(CudaData(a))]
    }
}

#[derive(LuminalPrint, Clone, LuminalEqFalse)]
pub struct CudaContiguous<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaContiguous<T> {
    pub fn new(
        shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (idx, valid) = get_idx_valid_exps(shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[shape]);
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel && ({valid}) != 0) {{
        out[idx] = inp_a[{idx}];
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}
impl<T: CudaFloat> Operator for CudaContiguous<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let res_shape = tensors[0].1.contiguous();
        let inp_size = res_shape.n_elements().to_usize().unwrap();
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);
        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaLog2<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaLog2<T> {
    pub fn new(device: Arc<CudaDevice>) -> Self {
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, int numel) {{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {{
        out[i] = log2(inp[i]);
    }}
}}"
        );
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
        }
    }
}

impl<T: CudaFloat> Operator for CudaLog2<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let inp_size = tensors[0].1.n_physical_elements().to_usize().unwrap();
        let mut out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig::for_num_elems(inp_size as u32),
                    (&mut out, inp, inp_size),
                )
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaExp2<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaExp2<T> {
    pub fn new(device: Arc<CudaDevice>) -> Self {
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, int numel) {{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {{
        out[i] = exp2(inp[i]);
    }}
}}"
        );
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
        }
    }
}
impl<T: CudaFloat> Operator for CudaExp2<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let inp_size = tensors[0].1.n_physical_elements().to_usize().unwrap();
        let mut out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig::for_num_elems(inp_size as u32),
                    (&mut out, inp, inp_size),
                )
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaSqrt<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaSqrt<T> {
    pub fn new(device: Arc<CudaDevice>) -> Self {
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, int numel) {{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {{
        out[i] = {}(inp[i]);
    }}
}}",
            if T::is_f32() { "sqrt" } else { "hsqrt" }
        );
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
        }
    }
}
impl<T: CudaFloat> Operator for CudaSqrt<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let inp_size = tensors[0].1.n_physical_elements().to_usize().unwrap();
        let mut out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig::for_num_elems(inp_size as u32),
                    (&mut out, inp, inp_size),
                )
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaSin<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaSin<T> {
    pub fn new(device: Arc<CudaDevice>) -> Self {
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, int numel) {{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {{
        out[i] = sin(inp[i]);
    }}
}}"
        );
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
        }
    }
}

impl<T: CudaFloat> Operator for CudaSin<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let inp_size = tensors[0].1.n_physical_elements().to_usize().unwrap();
        let mut out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig::for_num_elems(inp_size as u32),
                    (&mut out, inp, inp_size),
                )
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaRecip<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
}

impl<T: CudaFloat> CudaRecip<T> {
    pub fn new(device: Arc<CudaDevice>) -> Self {
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, int numel) {{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < numel) {{
        out[i] = {}(inp[i]);
    }}
}}",
            if T::is_f32() { "__frcp_rn" } else { "hrcp" }
        );
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
        }
    }
}

impl<T: CudaFloat> Operator for CudaRecip<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let inp_size = tensors[0].1.n_physical_elements().to_usize().unwrap();
        let mut out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        unsafe {
            self.function
                .clone()
                .launch(
                    LaunchConfig::for_num_elems(inp_size as u32),
                    (&mut out, inp, inp_size),
                )
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaAdd<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaAdd<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!(
            "
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        out[idx] =
            (({a_valid}) == 0 ? ({type_name})0.0 : inp_a[{a_idx}])
            + (({b_valid}) == 0 ? ({type_name})0.0 : inp_b[{b_idx}]);
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaAdd<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();
        let out = unsafe { self.device.alloc::<T>(inp_size).unwrap() };
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);

        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaMul<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaMul<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!("
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        out[idx] = (({a_valid}) == 0 ? ({type_name})0.0 : inp_a[{a_idx}]) * (({b_valid}) == 0 ? ({type_name})0.0 : inp_b[{b_idx}]);
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaMul<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();
        let out = unsafe { self.device.alloc::<T>(inp_size).unwrap() };
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);

        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaMod<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaMod<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!("
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        out[idx] = fmod((({a_valid}) == 0 ? ({type_name})0.0 : inp_a[{a_idx}]), (({b_valid}) == 0 ? ({type_name})0.0 : inp_b[{b_idx}]));
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaMod<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();
        let out = unsafe { self.device.alloc::<T>(inp_size).unwrap() };
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);

        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaLessThan<T> {
    function: CudaFunction,
    device: Arc<CudaDevice>,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaLessThan<T> {
    pub fn new(
        a_shape: ShapeTracker,
        b_shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (a_idx, a_valid) = get_idx_valid_exps(a_shape);
        let (b_idx, b_valid) = get_idx_valid_exps(b_shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[a_shape, b_shape]);
        let type_name = T::type_name();
        let code = format!("
#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp_a, const {type_name} *inp_b, int numel{rendered}) {{
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < numel) {{
        {type_name} a_t = (({a_valid}) != 0) ? inp_a[{a_idx}] : ({type_name})0.0;
        {type_name} b_t = (({b_valid}) != 0) ? inp_b[{b_idx}] : ({type_name})0.0;
        if (a_t < b_t) {{
            out[idx] = ({type_name})1.0;
        }} else {{
            out[idx] = ({type_name})0.0;
        }}
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}

impl<T: CudaFloat> Operator for CudaLessThan<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let a = get_buffer_from_tensor::<T>(&tensors[0].0);
        let b = get_buffer_from_tensor::<T>(&tensors[1].0);
        let inp_size = tensors[0].1.n_elements().to_usize().unwrap();
        let out = unsafe { self.device.alloc::<T>(inp_size).unwrap() };
        let mut params = vec![
            (&out).as_kernel_param(),
            a.as_kernel_param(),
            b.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);

        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }

        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaSumReduce<T> {
    function: CudaFunction,
    pub device: Arc<CudaDevice>,
    pub dim: usize,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaSumReduce<T> {
    pub fn new(
        dim: usize,
        shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (idx, valid) = get_idx_valid_exps(shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[shape]);
        let type_name = T::type_name();
        let code = format!("#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, const int front_size, const int back_size, const int dim_size, int numel{rendered}) {{
    int i_ = blockIdx.x * blockDim.x + threadIdx.x;

    if (i_ < numel) {{
        int a_ = i_ / back_size;
        int b_ = i_ % back_size;
        float reduce_value = 0.0;
        for (int c_ = 0; c_ < dim_size; c_++) {{
            int idx = a_ * dim_size * back_size + c_ * back_size + b_;
            if (({valid}) != 0) {{
                reduce_value = reduce_value + (float)inp[{idx}];
            }}
        }}
        out[i_] = ({type_name})reduce_value;
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            dim,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}
impl<T> Operator for CudaSumReduce<T>
where
    T: CudaFloat,
    CudaData<T>: Data,
{
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let mut shape = tensors[0].1;
        shape.remove_dim(self.dim);
        let inp_size = shape.n_elements().to_usize().unwrap();
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let front_size: usize = tensors[0]
            .1
            .shape()
            .iter()
            .take(self.dim)
            .map(|i| i.to_usize().unwrap())
            .product();
        let back_size: usize = tensors[0]
            .1
            .shape()
            .iter()
            .skip(self.dim + 1)
            .map(|i| i.to_usize().unwrap())
            .product();
        let dim_size = tensors[0].1.shape()[self.dim].to_usize().unwrap();

        let out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        let mut params = vec![
            (&out).as_kernel_param(),
            inp.as_kernel_param(),
            front_size.as_kernel_param(),
            back_size.as_kernel_param(),
            dim_size.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);
        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }
        vec![Tensor::new(CudaData(out))]
    }
}

#[derive(LuminalEqFalse, LuminalPrint, Clone)]
pub struct CudaMaxReduce<T> {
    function: CudaFunction,
    pub device: Arc<CudaDevice>,
    pub dim: usize,
    _phantom: PhantomData<T>,
    dyn_symbols: Vec<char>,
    dyn_map: *const FxHashMap<char, usize>,
}

impl<T: CudaFloat> CudaMaxReduce<T> {
    pub fn new(
        dim: usize,
        shape: ShapeTracker,
        device: Arc<CudaDevice>,
        dyn_map: *const FxHashMap<char, usize>,
    ) -> Self {
        let (idx, valid) = get_idx_valid_exps(shape);
        let (dyn_symbols, rendered) = render_dyn_dim_inputs(&[shape]);
        let type_name = T::type_name();
        let code = format!("#include \"cuda_fp16.h\"
extern \"C\" __global__ void kernel({type_name} *out, const {type_name} *inp, const int front_size, const int back_size, const int dim_size, int numel{rendered}) {{
    int i_ = blockIdx.x * blockDim.x + threadIdx.x;

    if (i_ < numel) {{
        int a_ = i_ / back_size;
        int b_ = i_ % back_size;
        float reduce_value = -__int_as_float(0x7f800000);
        for (int c_ = 0; c_ < dim_size; c_++) {{
            int idx = a_ * dim_size * back_size + c_ * back_size + b_;
            if (({valid}) != 0) {{
                reduce_value = max(reduce_value, (float)inp[{idx}]);
            }}
        }}
        out[i_] = ({type_name})reduce_value;
    }}
}}");
        Self {
            function: compile_and_load_kernel(code, &device),
            device,
            dim,
            _phantom: Default::default(),
            dyn_symbols,
            dyn_map,
        }
    }
}
impl<T: CudaFloat> Operator for CudaMaxReduce<T> {
    fn process(&mut self, tensors: Vec<(InputTensor, ShapeTracker)>) -> Vec<Tensor> {
        let mut shape = tensors[0].1;
        shape.remove_dim(self.dim);
        let inp_size = shape.n_elements().to_usize().unwrap();
        let inp = get_buffer_from_tensor::<T>(&tensors[0].0);
        let front_size: usize = tensors[0]
            .1
            .shape()
            .iter()
            .take(self.dim)
            .map(|i| i.to_usize().unwrap())
            .product();
        let back_size: usize = tensors[0]
            .1
            .shape()
            .iter()
            .skip(self.dim + 1)
            .map(|i| i.to_usize().unwrap())
            .product();
        let dim_size = tensors[0].1.shape()[self.dim].to_usize().unwrap();

        let out = self.device.alloc_zeros::<T>(inp_size).unwrap();
        let mut params = vec![
            (&out).as_kernel_param(),
            inp.as_kernel_param(),
            front_size.as_kernel_param(),
            back_size.as_kernel_param(),
            dim_size.as_kernel_param(),
            inp_size.as_kernel_param(),
        ];
        input_dyn_dims(&mut params, &self.dyn_symbols, self.dyn_map);
        unsafe {
            self.function
                .clone()
                .launch(LaunchConfig::for_num_elems(inp_size as u32), &mut params)
                .unwrap();
        }
        vec![Tensor::new(CudaData(out))]
    }
}

/// Convert all primitive ops to cuda primitive ops, and insert copy to and from device ops
#[derive(LuminalPrint, Default)]
pub struct CudaPrimitiveCompiler<T>(PhantomData<T>);

impl<T: CudaFloat> Compiler for CudaPrimitiveCompiler<T> {
    fn compile<To: ToIdsMut>(&self, graph: &mut Graph, mut remap: To) {
        let dev = CudaDevice::new(0).unwrap();
        // Go through the graph and insert copy ops
        // Copy function output to device and input from device
        for function_node in graph
            .node_indices()
            .filter(|n| {
                graph.node_weight(*n).unwrap().as_any().is::<Function>()
                    && graph.edges(*n).count() != 0
            })
            .collect::<Vec<_>>()
        {
            // Create copy node
            let copy_node = graph
                .add_op(CudaCopyToDevice::<T>::new(dev.clone()))
                .input(function_node, 0, ShapeTracker::new(&[]))
                .finish();

            // Switch outgoing edges from input to copy_node
            for (edge_id, weight, dest) in graph
                .edges_directed(function_node, petgraph::Direction::Outgoing)
                .map(|e| (e.id(), *e.weight(), e.target()))
                .filter(|(_, _, trg)| *trg != copy_node)
                .collect::<Vec<_>>()
            {
                graph.add_edge(copy_node, dest, weight);
                graph.remove_edge(edge_id);
            }

            if graph.to_retrieve.contains(&function_node) {
                graph.to_retrieve.insert(copy_node);
            }

            // Insert copy from device for function inputs
            for (source, edge, edge_weight) in graph
                .edges_directed(function_node, petgraph::Direction::Incoming)
                .map(|e| (e.source(), e.id(), *e.weight()))
                .collect::<Vec<_>>()
            {
                let copy_from_node = graph
                    .add_op(CudaCopyFromDevice::<T>::new(dev.clone()))
                    .input(source, 0, ShapeTracker::new(&[]))
                    .finish();
                graph.add_edge(copy_from_node, function_node, edge_weight);
                graph.remove_edge(edge);
            }
        }

        // Copy to_retrieve from device
        for (output_node, output_shape) in graph
            .to_retrieve
            .iter()
            // Filter to non-functions
            .filter(|n| !graph.node_weight(**n).unwrap().as_any().is::<LFunction>())
            .map(|n| {
                (
                    *n,
                    graph
                        .edges_directed(*n, petgraph::Direction::Incoming)
                        .filter_map(|e| e.weight().as_data())
                        .map(|i| i.2)
                        .max_by_key(|s| s.n_physical_elements().to_usize().unwrap_or_default())
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>()
        {
            if graph
                .node_weight(output_node)
                .unwrap()
                .as_any()
                .is::<CudaCopyToDevice<T>>()
            {
                // This output is already a copy to, instead of adding a copy from, let's remap back to the source
                let src = graph
                    .neighbors_directed(output_node, petgraph::Direction::Incoming)
                    .next()
                    .unwrap();
                graph.no_delete.remove(&output_node);
                graph.to_retrieve.remove(&output_node);
                graph.no_delete.insert(src);
                graph.to_retrieve.insert(src);
            } else {
                // Create copy node
                let copy_node = graph
                    .add_op(CudaCopyFromDevice::<T>::new(dev.clone()))
                    .input(output_node, 0, output_shape)
                    .finish();

                move_references(
                    &mut remap,
                    &mut graph.no_delete,
                    &mut graph.to_retrieve,
                    output_node,
                    copy_node,
                );
            }
        }

        // Copy prints from device
        for (output_node, edge) in graph
            .node_indices()
            // Filter non-functions
            .filter(|n| graph.node_weight(*n).unwrap().as_any().is::<Print>())
            .map(|n| {
                (
                    n,
                    graph
                        .edges_directed(n, petgraph::Direction::Incoming)
                        .find(|e| !e.weight().is_schedule())
                        .unwrap()
                        .id(),
                )
            })
            .collect::<Vec<_>>()
        {
            // Create copy node
            let (source, shape) = (
                graph.edge_endpoints(edge).unwrap().0,
                graph.edge_weight(edge).unwrap().as_data().unwrap().2,
            );
            let copy_node = graph
                .add_op(CudaCopyFromDevice::<T>::new(dev.clone()))
                .input(source, 0, shape)
                .finish();
            graph.add_edge(
                copy_node,
                output_node,
                Dependency::Data {
                    shape,
                    input_order: 0,
                    output_order: 0,
                },
            );
            graph.remove_edge(edge);
        }

        fn is<T: Any>(type_id: TypeId) -> bool {
            type_id == TypeId::of::<T>()
        }

        // Swap primitive ops
        for id in graph.node_indices().collect::<Vec<_>>() {
            let shapes = graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .filter_map(|i| i.weight().as_data())
                .sorted_by_key(|e| e.0)
                .map(|e| e.2)
                .collect::<Vec<_>>();
            let op = graph.node_weight(id).unwrap().as_any().type_id();
            let op_ref = graph.graph.node_weight_mut(id).unwrap();
            if is::<Log2>(op) {
                *op_ref = Box::new(CudaLog2::<T>::new(dev.clone()));
            } else if is::<Exp2>(op) {
                *op_ref = Box::new(CudaExp2::<T>::new(dev.clone()));
            } else if is::<Sin>(op) {
                *op_ref = Box::new(CudaSin::<T>::new(dev.clone()));
            } else if let Some(c) = op_ref.as_any().downcast_ref::<Constant>() {
                *op_ref = Box::new(CudaConstant::<T>::new(
                    dev.clone(),
                    c.0.clone(),
                    &graph.dyn_map,
                ));
            } else if is::<Recip>(op) {
                *op_ref = Box::new(CudaRecip::<T>::new(dev.clone()));
            } else if is::<Sqrt>(op) {
                *op_ref = Box::new(CudaSqrt::<T>::new(dev.clone()));
            } else if is::<Add>(op) {
                *op_ref = Box::new(CudaAdd::<T>::new(
                    shapes[0],
                    shapes[1],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if is::<Mul>(op) {
                *op_ref = Box::new(CudaMul::<T>::new(
                    shapes[0],
                    shapes[1],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if is::<Mod>(op) {
                *op_ref = Box::new(CudaMod::<T>::new(
                    shapes[0],
                    shapes[1],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if is::<LessThan>(op) {
                *op_ref = Box::new(CudaLessThan::<T>::new(
                    shapes[0],
                    shapes[1],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if is::<Contiguous>(op) {
                *op_ref = Box::new(CudaContiguous::<T>::new(
                    shapes[0],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if let Some(SumReduce(dim)) = op_ref.as_any().downcast_ref() {
                *op_ref = Box::new(CudaSumReduce::<T>::new(
                    *dim,
                    shapes[0],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            } else if let Some(MaxReduce(dim)) = op_ref.as_any().downcast_ref() {
                *op_ref = Box::new(CudaMaxReduce::<T>::new(
                    *dim,
                    shapes[0],
                    dev.clone(),
                    &graph.dyn_map,
                ));
            }
        }
    }
}

// Sometimes CopyTo -> CopyFrom and CopyFrom -> CopyTo patterns remain, so let's clean them up
#[derive(Debug, Default)]
pub struct CopyCompiler<T>(PhantomData<T>);

impl<T: CudaFloat> Compiler for CopyCompiler<T> {
    fn compile<To: ToIdsMut>(&self, graph: &mut Graph, mut remap: To) {
        for (first, second) in graph
            .edge_indices()
            .filter_map(|e| graph.edge_endpoints(e))
            .filter(|(a, b)| {
                (graph
                    .node_weight(*a)
                    .unwrap()
                    .as_any()
                    .is::<CudaCopyToDevice<T>>()
                    && graph
                        .node_weight(*b)
                        .unwrap()
                        .as_any()
                        .is::<CudaCopyFromDevice<T>>())
                    || (graph
                        .node_weight(*a)
                        .unwrap()
                        .as_any()
                        .is::<CudaCopyFromDevice<T>>()
                        && graph
                            .node_weight(*b)
                            .unwrap()
                            .as_any()
                            .is::<CudaCopyToDevice<T>>())
            })
            .unique_by(|n| n.0)
            .unique_by(|n| n.1)
            .collect::<Vec<_>>()
        {
            if graph
                .edges_directed(first, petgraph::Direction::Outgoing)
                .filter(|e| graph.contains_node(e.target()))
                .filter(|e| {
                    !graph
                        .node_weight(e.target())
                        .unwrap()
                        .as_any()
                        .is::<CudaCopyFromDevice<T>>()
                        && !graph
                            .node_weight(e.target())
                            .unwrap()
                            .as_any()
                            .is::<CudaCopyToDevice<T>>()
                })
                .count()
                > 0
                || graph.no_delete.contains(&first)
            {
                continue;
            }
            let source = graph.get_sources(first)[0];
            move_outgoing_edge(second, source.0, graph);
            move_references(
                &mut remap,
                &mut graph.no_delete,
                &mut graph.to_retrieve,
                second,
                source.0,
            );
            graph.remove_node(second);
            for dest in graph
                .get_dests(first)
                .iter()
                .map(|(i, _)| *i)
                .collect::<Vec<_>>()
            {
                move_outgoing_edge(dest, source.0, graph);
                move_references(
                    &mut remap,
                    &mut graph.no_delete,
                    &mut graph.to_retrieve,
                    dest,
                    source.0,
                );
                graph.remove_node(dest);
            }
            graph.remove_node(first);
        }
    }
}
