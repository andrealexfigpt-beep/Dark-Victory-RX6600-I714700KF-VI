//! Direct resource binding optimizer (Phase 3 Native Graphics).
//!
//! Optimizes resource binding by using direct root descriptors where
//! semantically equivalent and safe, falling back to descriptor tables
//! with persistent heap allocation when bounds checking is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::host::host;
use crate::host::descriptor_pool::{allocate_cbv_srv_uav, free_cbv_srv_uav, DescriptorBlock};

/// A resource binding that can be either direct or table-based.
#[derive(Debug, Clone)]
pub(crate) enum ResourceBinding {
    /// Direct root descriptor (no bounds checking).
    DirectRootDescriptor {
        shader_register: u32,
        register_space: u32,
        root_param_index: u32,
    },
    /// Descriptor table with persistent heap allocation.
    DescriptorTable {
        heap_block: DescriptorBlock,
        base_shader_register: u32,
        register_space: u32,
        root_param_index: u32,
        num_descriptors: u32,
    },
    /// Static sampler.
    StaticSampler {
        shader_register: u32,
        register_space: u32,
    },
}

/// Configuration for binding optimization.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BindingConfig {
    /// Prefer direct root descriptors when safe (default: true for native).
    pub(crate) prefer_direct: bool,
    /// Enable bounds checking via descriptor tables.
    pub(crate) enable_bounds_check: bool,
    /// Maximum descriptors per table before splitting.
    pub(crate) max_table_descriptors: u32,
}

impl BindingConfig {
    /// Default configuration for native graphics path.
    pub(crate) fn native_default() -> Self {
        Self {
            prefer_direct: true,
            enable_bounds_check: false, // We manage bounds for our resources
            max_table_descriptors: 256,
        }
    }

    /// Compatibility configuration (legacy behavior).
    pub(crate) fn compatibility() -> Self {
        Self {
            prefer_direct: false,
            enable_bounds_check: true,
            max_table_descriptors: 256,
        }
    }
}

/// Global binding configuration.
static BINDING_CONFIG: OnceLock<BindingConfig> = OnceLock::new();

/// Set the binding configuration (call once at startup).
pub(crate) fn set_binding_config(config: BindingConfig) {
    BINDING_CONFIG.set(config).ok();
}

/// Get the current binding configuration.
pub(crate) fn binding_config() -> &'static BindingConfig {
    BINDING_CONFIG.get_or_init(|| {
        // Check environment variable for override
        if std::env::var_os("VICTORY_BINDING_DIRECT").is_some_and(|v| v == "0") {
            BindingConfig::compatibility()
        } else if std::env::var_os("VICTORY_BINDING_BOUNDED").is_some_and(|v| v == "1") {
            BindingConfig {
                prefer_direct: false,
                enable_bounds_check: true,
                max_table_descriptors: 256,
            }
        } else {
            BindingConfig::native_default()
        }
    })
}

/// A resource view with known bounds.
#[derive(Debug, Clone)]
pub(crate) struct BoundedResourceView {
    pub(crate) gpu_address: u64,
    pub(crate) size_in_bytes: u64,
    pub(crate) format: DXGI_FORMAT,
    pub(crate) stride: u64,
}

/// Registry of resources with known bounds for safe direct binding.
pub(crate) struct BoundedResourceRegistry {
    /// Map from GPU address to resource bounds.
    resources: Mutex<HashMap<u64, BoundedResourceView>>,
}

impl BoundedResourceRegistry {
    fn new() -> Self {
        Self {
            resources: Mutex::new(HashMap::new()),
        }
    }

    /// Register a resource with known bounds.
    pub(crate) fn register(&self, view: BoundedResourceView) {
        self.resources.lock().unwrap().insert(view.gpu_address, view);
    }

    /// Unregister a resource.
    pub(crate) fn unregister(&self, gpu_address: u64) {
        self.resources.lock().unwrap().remove(&gpu_address);
    }

    /// Check if a resource at the given address has known bounds.
    pub(crate) fn has_bounds(&self, gpu_address: u64) -> bool {
        self.resources.lock().unwrap().contains_key(&gpu_address)
    }

    /// Get the bounds for a resource.
    pub(crate) fn get_bounds(&self, gpu_address: u64) -> Option<BoundedResourceView> {
        self.resources.lock().unwrap().get(&gpu_address).cloned()
    }
}

static BOUNDED_REGISTRY: OnceLock<BoundedResourceRegistry> = OnceLock::new();

/// Get the bounded resource registry.
pub(crate) fn bounded_registry() -> &'static BoundedResourceRegistry {
    BOUNDED_REGISTRY.get_or_init(BoundedResourceRegistry::new)
}

/// Determine if a resource can use direct binding.
/// Returns true if direct binding is safe (bounds known or not needed).
pub(crate) fn can_use_direct_binding(
    gpu_address: u64,
    is_srv: bool,
    is_uav: bool,
) -> bool {
    let config = binding_config();
    
    if !config.prefer_direct {
        return false;
    }

    // If bounds checking is explicitly enabled, use tables.
    if config.enable_bounds_check {
        return false;
    }

    // For our managed resources (native memory), we know bounds.
    if bounded_registry().has_bounds(gpu_address) {
        return true;
    }

    // For UAVs, we need bounds checking to prevent out-of-bounds writes.
    if is_uav {
        return false;
    }

    // For SRVs, if we don't know bounds, be conservative.
    // But for native path, we assume title resources are well-behaved.
    // This is controlled by VICTORY_BINDING_DIRECT=0 to disable.
    true
}

/// Check if RDNA2-specific binding optimizations are available.
/// RDNA2 (Navi 2x) supports direct binding with better performance.
pub(crate) fn rdna2_binding_optimized() -> bool {
    crate::host::memory_mgr::rebar_supported() && crate::host::memory_mgr::upload_heap_supported()
}

/// Get the optimal binding strategy for the current hardware.
pub(crate) fn optimal_binding_strategy(is_srv: bool, is_uav: bool) -> BindingStrategy {
    if rdna2_binding_optimized() {
        // RDNA2 with ReBAR: prefer direct binding, use descriptor tables only when necessary
        if is_uav {
            BindingStrategy::DirectWithBoundsCheck
        } else {
            BindingStrategy::Direct
        }
    } else if crate::host::memory_mgr::upload_heap_supported() {
        // GPU upload heap available but no ReBAR
        BindingStrategy::DirectWithBoundsCheck
    } else {
        // Fallback: use descriptor tables for safety
        BindingStrategy::DescriptorTable
    }
}

/// Binding strategy for resource binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingStrategy {
    /// Direct root descriptor binding (fastest).
    Direct,
    /// Direct binding with bounds checking via descriptor table.
    DirectWithBoundsCheck,
    /// Full descriptor table (safest, compatible).
    DescriptorTable,
}

/// Create a root parameter for a resource binding.
pub(crate) fn create_root_parameter(
    binding: &ResourceBinding,
    visibility: D3D12_SHADER_VISIBILITY,
) -> D3D12_ROOT_PARAMETER1 {
    match binding {
        ResourceBinding::DirectRootDescriptor { shader_register, register_space, .. } => {
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV, // Default, overridden by caller
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                        ShaderRegister: *shader_register,
                        RegisterSpace: *register_space,
                        Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                    },
                },
                ShaderVisibility: visibility,
            }
        }
        ResourceBinding::DescriptorTable { base_shader_register, register_space, num_descriptors, .. } => {
            // The actual descriptor range is set up by the caller using the heap block.
            D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: std::ptr::null(), // Filled by caller
                    },
                },
                ShaderVisibility: visibility,
            }
        }
        ResourceBinding::StaticSampler { .. } => {
            // Static samplers are handled separately.
            D3D12_ROOT_PARAMETER1::default()
        }
    }
}

/// Allocate a descriptor table block for a resource.
pub(crate) fn allocate_descriptor_table(
    count: u32,
    is_sampler: bool,
) -> windows::core::Result<DescriptorBlock> {
    if is_sampler {
        crate::host::descriptor_pool::allocate_samplers(count)
    } else {
        crate::host::descriptor_pool::allocate_cbv_srv_uav(count)
    }
}

/// Free a descriptor table block.
pub(crate) fn free_descriptor_table(block: DescriptorBlock, is_sampler: bool) {
    if is_sampler {
        crate::host::descriptor_pool::free_samplers(block);
    } else {
        crate::host::descriptor_pool::free_cbv_srv_uav(block);
    }
}

/// Record a descriptor in a table block.
pub(crate) fn write_descriptor(
    block: &DescriptorBlock,
    index: u32,
    view: &D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if index >= block.count {
        return;
    }
    let dest = D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: block.cpu_handle.ptr + (index as usize * crate::host::descriptor_pool::cbv_srv_uav_pool().descriptor_size() as usize),
    };
    // SAFETY: descriptor heaps are just memory.
    unsafe { std::ptr::copy_nonoverlapping(view.ptr as *const u8, dest.ptr as *mut u8, crate::host::descriptor_pool::cbv_srv_uav_pool().descriptor_size() as usize) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binding_config_defaults() {
        let config = BindingConfig::native_default();
        assert!(config.prefer_direct);
        assert!(!config.enable_bounds_check);
        
        let compat = BindingConfig::compatibility();
        assert!(!compat.prefer_direct);
        assert!(compat.enable_bounds_check);
    }

    #[test]
    fn test_resource_binding_enum() {
        let direct = ResourceBinding::DirectRootDescriptor {
            shader_register: 0,
            register_space: 0,
            root_param_index: 0,
        };
        
        let table = ResourceBinding::DescriptorTable {
            heap_block: DescriptorBlock {
                heap_index: 0,
                start: 0,
                count: 1,
                cpu_handle: D3D12_CPU_DESCRIPTOR_HANDLE { ptr: 0 },
                gpu_handle: D3D12_GPU_DESCRIPTOR_HANDLE { ptr: 0 },
            },
            base_shader_register: 0,
            register_space: 0,
            root_param_index: 0,
            num_descriptors: 1,
        };
        
        assert!(matches!(direct, ResourceBinding::DirectRootDescriptor { .. }));
        assert!(matches!(table, ResourceBinding::DescriptorTable { .. }));
    }
}