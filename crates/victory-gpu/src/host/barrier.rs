//! Redundant resource barrier elimination (Phase 3 Native Graphics).
//!
//! Tracks resource states across command lists and submissions to
//! eliminate redundant barriers. Integrates with existing barrier system.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::host;

/// The known state of a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResourceState {
    Common,
    VertexAndConstantBuffer,
    IndexBuffer,
    RenderTarget,
    UnorderedAccess,
    DepthWrite,
    DepthRead,
    NonPixelShaderResource,
    PixelShaderResource,
    StreamOutput,
    IndirectArgument,
    CopyDest,
    CopySource,
    ResolveDest,
    ResolveSource,
    GenericRead,
    Present,
    Predication,
    RaytracingAccelerationStructure,
    ShadingRateSource,
}

/// A resource with its current state and subresource states.
#[derive(Debug, Clone)]
pub(crate) struct TrackedResource {
    pub(crate) resource: ID3D12Resource,
    pub(crate) global_state: ResourceState,
    /// Per-subresource states (for textures with multiple subresources).
    pub(crate) subresource_states: HashMap<u32, ResourceState>,
    /// Last submission this was used in.
    pub(crate) last_submission: u64,
    /// Whether the state is known (vs assumed).
    pub(crate) known: bool,
}

impl TrackedResource {
    fn new(resource: ID3D12Resource, initial_state: ResourceState) -> Self {
        Self {
            resource,
            global_state: initial_state,
            subresource_states: HashMap::new(),
            last_submission: 0,
            known: true,
        }
    }

    /// Get the state for a specific subresource.
    fn get_state(&self, subresource: u32) -> ResourceState {
        self.subresource_states.get(&subresource).copied().unwrap_or(self.global_state)
    }

    /// Set the state for a specific subresource.
    fn set_state(&mut self, subresource: u32, state: ResourceState) {
        if subresource == 0xFFFFFFFF {
            self.global_state = state;
            self.subresource_states.clear();
        } else {
            self.subresource_states.insert(subresource, state);
        }
    }

    /// Check if a transition is needed.
    fn needs_transition(&self, subresource: u32, target_state: ResourceState) -> bool {
        self.get_state(subresource) != target_state
    }
}

/// Resource state tracker for barrier elimination.
pub(crate) struct BarrierTracker {
    /// Tracked resources by pointer address.
    resources: Mutex<HashMap<usize, TrackedResource>>,
    /// Current submission number.
    current_submission: AtomicU64,
    /// Statistics.
    stats: BarrierStats,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct BarrierStats {
    pub barriers_emitted: u64,
    pub barriers_eliminated: u64,
    pub state_transitions: u64,
    pub unknown_resources: u64,
}

impl BarrierTracker {
    fn new() -> Self {
        Self {
            resources: Mutex::new(HashMap::new()),
            current_submission: AtomicU64::new(0),
            stats: BarrierStats::default(),
        }
    }

    /// Start a new submission.
    pub(crate) fn new_submission(&self) -> u64 {
        self.current_submission.fetch_add(1, Ordering::AcqRel)
    }

    /// Get the current submission number.
    pub(crate) fn current_submission(&self) -> u64 {
        self.current_submission.load(Ordering::Acquire)
    }

    /// Register a resource with an initial state.
    pub(crate) fn register(&self, resource: &ID3D12Resource, initial_state: ResourceState) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        resources.entry(ptr).or_insert_with(|| TrackedResource::new(resource.clone(), initial_state));
    }

    /// Record that a resource is used in a specific state (for tracking).
    pub(crate) fn use_resource(&self, resource: &ID3D12Resource, state: ResourceState, subresource: u32) {
        let ptr = resource.as_raw() as usize;
        let mut resources = self.resources.lock().unwrap();
        let entry = resources.entry(ptr).or_insert_with(|| {
            self.stats.unknown_resources += 1;
            TrackedResource::new(resource.clone(), ResourceState::Common)
        });
        entry.last_submission = self.current_submission.load(Ordering::Acquire);
        entry.set_state(subresource, state);
        entry.known = true;
    }

    /// Emit necessary barriers for a list of required states.
    /// Returns the barriers that need to be emitted.
    pub(crate) fn emit_barriers(
        &self,
        required: &[(ID3D12Resource, ResourceState, u32)],
    ) -> Vec<D3D12_RESOURCE_BARRIER> {
        let mut resources = self.resources.lock().unwrap();
        let mut barriers = Vec::new();

        for (resource, target_state, subresource) in required {
            let ptr = resource.as_raw() as usize;
            
            let entry = resources.entry(ptr).or_insert_with(|| {
                self.stats.unknown_resources += 1;
                TrackedResource::new(resource.clone(), ResourceState::Common)
            });

            // Check if transition is needed.
            let current_state = entry.get_state(*subresource);
            if current_state != *target_state {
                // Emit barrier.
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAGS(0),
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                            Subresource: *subresource,
                            StateBefore: d3d12_state(current_state),
                            StateAfter: d3d12_state(*target_state),
                        })
                    }
                };
                barriers.push(barrier);
                
                // Update tracked state.
                entry.set_state(*subresource, *target_state);
                entry.last_submission = self.current_submission.load(Ordering::Acquire);
                entry.known = true;
                
                self.stats.barriers_emitted += 1;
                self.stats.state_transitions += 1;
            } else {
                // No barrier needed - state already matches.
                self.stats.barriers_eliminated += 1;
            }
        }

        barriers
    }

    /// Emit a UAV barrier for a resource.
    pub(crate) fn emit_uav_barrier(&self, resource: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAGS(0),
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                })
            }
        }
    }

    /// Emit an aliasing barrier.
    pub(crate) fn emit_aliasing_barrier(
        &self,
        before: Option<&ID3D12Resource>,
        after: Option<&ID3D12Resource>,
    ) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAGS(0),
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: std::mem::ManuallyDrop::new(before.cloned()),
                    pResourceAfter: std::mem::ManuallyDrop::new(after.cloned()),
                })
            }
        }
    }

    /// Get statistics.
    pub(crate) fn stats(&self) -> BarrierStats {
        self.stats
    }

    /// Clean up resources not used in recent submissions.
    pub(crate) fn cleanup_old(&self, threshold: u64) {
        let current = self.current_submission.load(Ordering::Acquire);
        let mut resources = self.resources.lock().unwrap();
        resources.retain(|_, r| current.saturating_sub(r.last_submission) < threshold);
    }
}

/// Global barrier tracker instance.
static BARRIER_TRACKER: OnceLock<BarrierTracker> = OnceLock::new();

/// Get the global barrier tracker.
pub(crate) fn barrier_tracker() -> &'static BarrierTracker {
    BARRIER_TRACKER.get_or_init(BarrierTracker::new)
}

/// Initialize the barrier tracker.
pub(crate) fn init_barrier_tracker() {
    BARRIER_TRACKER.get_or_init(BarrierTracker::new);
}

/// Convert our ResourceState to D3D12_RESOURCE_STATES.
fn d3d12_state(state: ResourceState) -> D3D12_RESOURCE_STATES {
    match state {
        ResourceState::Common => D3D12_RESOURCE_STATE_COMMON,
        ResourceState::VertexAndConstantBuffer => D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER,
        ResourceState::IndexBuffer => D3D12_RESOURCE_STATE_INDEX_BUFFER,
        ResourceState::RenderTarget => D3D12_RESOURCE_STATE_RENDER_TARGET,
        ResourceState::UnorderedAccess => D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        ResourceState::DepthWrite => D3D12_RESOURCE_STATE_DEPTH_WRITE,
        ResourceState::DepthRead => D3D12_RESOURCE_STATE_DEPTH_READ,
        ResourceState::NonPixelShaderResource => D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        ResourceState::PixelShaderResource => D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        ResourceState::StreamOutput => D3D12_RESOURCE_STATE_STREAM_OUT,
        ResourceState::IndirectArgument => D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        ResourceState::CopyDest => D3D12_RESOURCE_STATE_COPY_DEST,
        ResourceState::CopySource => D3D12_RESOURCE_STATE_COPY_SOURCE,
        ResourceState::ResolveDest => D3D12_RESOURCE_STATE_RESOLVE_DEST,
        ResourceState::ResolveSource => D3D12_RESOURCE_STATE_RESOLVE_SOURCE,
        ResourceState::GenericRead => D3D12_RESOURCE_STATE_GENERIC_READ,
        ResourceState::Present => D3D12_RESOURCE_STATE_PRESENT,
        ResourceState::Predication => D3D12_RESOURCE_STATE_PREDICATION,
        ResourceState::RaytracingAccelerationStructure => D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        ResourceState::ShadingRateSource => D3D12_RESOURCE_STATE_SHADING_RATE_SOURCE,
    }
}

/// Helper to emit barriers for a command list using the tracker.
pub(crate) fn emit_tracked_barriers(
    list: &ID3D12GraphicsCommandList,
    required: &[(ID3D12Resource, ResourceState, u32)],
) -> u32 {
    let tracker = barrier_tracker();
    let barriers = tracker.emit_barriers(required);
    if !barriers.is_empty() {
        unsafe { list.ResourceBarrier(&barriers) };
    }
    barriers.len() as u32
}

/// Start a new submission for barrier tracking.
pub(crate) fn barrier_new_submission() -> u64 {
    barrier_tracker().new_submission()
}

/// Register a resource with the barrier tracker.
pub(crate) fn barrier_register(resource: &ID3D12Resource, initial_state: ResourceState) {
    barrier_tracker().register(resource, initial_state);
}

/// Record resource usage for tracking.
pub(crate) fn barrier_use(resource: &ID3D12Resource, state: ResourceState, subresource: u32) {
    barrier_tracker().use_resource(resource, state, subresource);
}

/// Get barrier statistics.
pub(crate) fn barrier_stats() -> BarrierStats {
    barrier_tracker().stats()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_state_conversion() {
        assert_eq!(d3d12_state(ResourceState::Common), D3D12_RESOURCE_STATE_COMMON);
        assert_eq!(d3d12_state(ResourceState::RenderTarget), D3D12_RESOURCE_STATE_RENDER_TARGET);
        assert_eq!(d3d12_state(ResourceState::UnorderedAccess), D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        assert_eq!(d3d12_state(ResourceState::DepthWrite), D3D12_RESOURCE_STATE_DEPTH_WRITE);
        assert_eq!(d3d12_state(ResourceState::CopyDest), D3D12_RESOURCE_STATE_COPY_DEST);
    }

    #[test]
    fn test_barrier_tracker_basic() {
        let tracker = BarrierTracker::new();
        let submission = tracker.new_submission();
        assert_eq!(submission, 1);
        
        let stats = tracker.stats();
        assert_eq!(stats.barriers_emitted, 0);
        assert_eq!(stats.barriers_eliminated, 0);
    }
}