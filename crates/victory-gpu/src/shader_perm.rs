//! Shader permutation management for ahead-of-time compilation.
//!
//! Tracks all shader permutations seen at runtime, deduplicates them by their
//! translation-affecting parameters, and provides the data needed for offline
//! precompilation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::{HwStage, RootType};

/// A shader permutation key that uniquely identifies a translated shader
/// based on all parameters that affect translation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShaderPermutationKey {
    /// Stage of the shader
    pub stage: HwStage,
    /// BLAKE3 hash of the original DXBC container
    pub container_hash: [u8; 32],
    /// Translation fingerprint (translator version + all VICTORY_* settings)
    pub translation_fingerprint: [u8; 32],
    /// Root signature hash (affects resource binding layout)
    pub root_signature_hash: [u8; 32],
    /// Whether this is a wave64 program on a 32-lane host
    pub wave_mismatch: bool,
    /// Whether the program uses cross-lane operations
    pub cross_lane: bool,
}

impl ShaderPermutationKey {
    /// Create a key from a translated shader and its context.
    pub fn from_shader(
        stage: HwStage,
        container: &[u8],
        translation_fingerprint: Vec<u8>,
        root_signature_hash: [u8; 32],
        wave_mismatch: bool,
        cross_lane: bool,
    ) -> Self {
        let container_hash = blake3::hash(container).as_bytes().clone();
        ShaderPermutationKey {
            stage,
            container_hash,
            translation_fingerprint,
            root_signature_hash,
            wave_mismatch,
            cross_lane,
        }
    }

    /// Get a short hex identifier for logging.
    pub fn short_id(&self) -> String {
        let mut h = Hasher::new();
        h.update(&self.container_hash);
        h.update(&self.translation_fingerprint);
        h.update(&self.root_signature_hash);
        h.update(&[self.wave_mismatch as u8, self.cross_lane as u8]);
        h.finalize().to_hex()[..16].to_string()
    }

    /// Get a filename-safe identifier.
    pub fn file_id(&self) -> String {
        let mut h = Hasher::new();
        h.update(&self.container_hash);
        h.update(&self.translation_fingerprint);
        h.update(&self.root_signature_hash);
        h.update(&[self.wave_mismatch as u8, self.cross_lane as u8]);
        format!("{}_{}", self.stage_str(), h.finalize().to_hex())
    }

    fn stage_str(&self) -> &'static str {
        match self.stage {
            HwStage::Vs => "vs",
            HwStage::Ps => "ps",
            HwStage::Cs => "cs",
            HwStage::Ms => "ms",
            HwStage::Ls => "ls",
            HwStage::Hs => "hs",
            HwStage::Es => "es",
            HwStage::Gs => "gs",
        }
    }
}

/// A recorded shader permutation with its translation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShaderPermutation {
    pub key: ShaderPermutationKey,
    /// The translated DXIL container
    pub dxil: Vec<u8>,
    /// User data layout
    pub user_data: Vec<UserDataRecord>,
    /// Immediate samplers
    pub samplers: Vec<[u32; 4]>,
    /// Static samplers
    pub static_samplers: Vec<StaticSamplerRecord>,
    /// Root CBV/SRV/UAV indices
    pub root_cbvs: Vec<u8>,
    pub root_srvs: Vec<u8>,
    pub root_uavs: Vec<u8>,
    /// Root indexed views
    pub root_indexed_views: Vec<RootIndexedViewRecord>,
    /// When this permutation was first seen
    pub first_seen_frame: u64,
    /// How many times this permutation was used
    pub use_count: u64,
    /// Whether this permutation successfully created a PSO
    pub pso_created: bool,
    /// Pipeline keys that use this shader
    pub pipeline_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDataRecord {
    pub index: u32,
    pub slot: u8,
    pub root: u32, // RootType as u32
    pub dword: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaticSamplerRecord {
    pub filter: u32,
    pub address: [u32; 3],
    pub mip_lod_bias: u32, // f32 bits
    pub max_anisotropy: u32,
    pub comparison: u32,
    pub border: u32,
    pub min_lod: u32, // f32 bits
    pub max_lod: u32, // f32 bits
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIndexedViewRecord {
    pub table_slot: u8,
    pub constant_slot: u8,
    pub constant_dword: u32,
    pub byte_offset: u32,
    pub shift: u8,
}

impl From<&HostShader> for ShaderPermutation {
    fn from(shader: &HostShader) -> Self {
        ShaderPermutation {
            key: ShaderPermutationKey {
                stage: shader.stage,
                container_hash: [0; 32], // Filled in by caller
                translation_fingerprint: [0; 32], // Filled in by caller
                root_signature_hash: [0; 32], // Filled in by caller
                wave_mismatch: false, // Filled in by caller
                cross_lane: false, // Filled in by caller
            },
            dxil: shader.dxil.clone(),
            user_data: shader.user_data.iter().map(|u| UserDataRecord {
                index: u.index,
                slot: u.slot,
                root: root_type_to_u32(u.root),
                dword: u.dword,
            }).collect(),
            samplers: shader.samplers.clone(),
            static_samplers: shader.static_samplers.iter().map(|s| StaticSamplerRecord {
                filter: s.filter,
                address: s.address,
                mip_lod_bias: s.mip_lod_bias.to_bits(),
                max_anisotropy: s.max_anisotropy,
                comparison: s.comparison,
                border: s.border,
                min_lod: s.min_lod.to_bits(),
                max_lod: s.max_lod.to_bits(),
            }).collect(),
            root_cbvs: shader.root_cbvs.clone(),
            root_srvs: shader.root_srvs.clone(),
            root_uavs: shader.root_uavs.clone(),
            root_indexed_views: shader.root_indexed_views.iter().map(|v| RootIndexedViewRecord {
                table_slot: v.table_slot,
                constant_slot: v.constant_slot,
                constant_dword: v.constant_dword,
                byte_offset: v.byte_offset,
                shift: v.shift,
            }).collect(),
            first_seen_frame: 0,
            use_count: 0,
            pso_created: false,
            pipeline_keys: Vec::new(),
        }
    }
}

fn root_type_to_u32(rt: RootType) -> u32 {
    match rt {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn u32_to_root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

impl ShaderPermutation {
    /// Reconstruct a HostShader from this permutation.
    pub fn to_host_shader(&self) -> HostShader {
        HostShader {
            stage: self.key.stage,
            dxil: self.dxil.clone(),
            user_data: self.user_data.iter().map(|u| UserData {
                index: u.index,
                slot: u.slot,
                root: u32_to_root_type(u.root),
                dword: u.dword,
            }).collect(),
            samplers: self.samplers.clone(),
            static_samplers: self.static_samplers.iter().map(|s| StaticSampler {
                filter: s.filter,
                address: s.address,
                mip_lod_bias: f32::from_bits(s.mip_lod_bias),
                max_anisotropy: s.max_anisotropy,
                comparison: s.comparison,
                border: s.border,
                min_lod: f32::from_bits(s.min_lod),
                max_lod: f32::from_bits(s.max_lod),
            }).collect(),
            root_cbvs: self.root_cbvs.clone(),
            root_srvs: self.root_srvs.clone(),
            root_uavs: self.root_uavs.clone(),
            root_indexed_views: self.root_indexed_views.iter().map(|v| victory_shader::resource::RootIndexedView {
                table_slot: v.table_slot,
                constant_slot: v.constant_slot,
                constant_dword: v.constant_dword,
                byte_offset: v.byte_offset,
                shift: v.shift,
            }).collect(),
        }
    }
}

/// Global shader permutation tracker.
struct PermutationTracker {
    /// All permutations seen, keyed by their permutation key
    permutations: RwLock<HashMap<ShaderPermutationKey, ShaderPermutation>>,
    /// Pipeline keys that use each shader permutation
    pipeline_shader_map: RwLock<HashMap<String, Vec<ShaderPermutationKey>>>,
    /// Frame counter
    frame: std::sync::atomic::AtomicU64,
    /// Whether to persist permutations to disk
    persist_enabled: bool,
    /// Persistence directory
    persist_dir: Mutex<Option<PathBuf>>,
}

impl PermutationTracker {
    fn new() -> Self {
        PermutationTracker {
            permutations: RwLock::new(HashMap::new()),
            pipeline_shader_map: RwLock::new(HashMap::new()),
            frame: std::sync::atomic::AtomicU64::new(0),
            persist_enabled: false,
            persist_dir: Mutex::new(None),
        }
    }

    /// Enable persistence to disk.
    pub fn enable_persistence(&self, dir: PathBuf) {
        *self.persist_dir.lock().unwrap() = Some(dir);
        self.persist_enabled = true;
    }

    /// Record a shader translation.
    pub fn record_translation(
        &self,
        stage: HwStage,
        container: &[u8],
        translation_fingerprint: Vec<u8>,
        root_signature_hash: [u8; 32],
        wave_mismatch: bool,
        cross_lane: bool,
        shader: &HostShader,
    ) -> ShaderPermutationKey {
        let key = ShaderPermutationKey::from_shader(
            stage, container, translation_fingerprint, root_signature_hash,
            false, false,
        );

        let mut perms = self.permutations.write().unwrap();
        let perm = perms.entry(key.clone()).or_insert_with(|| {
            let mut p = ShaderPermutation::from(shader);
            p.key = key.clone();
            p.first_seen_frame = self.frame.load(std::sync::atomic::Ordering::Relaxed);
            p
        });
        perm.use_count += 1;
        key
    }

    /// Record that a pipeline uses a shader permutation.
    pub fn record_pipeline_shader(&self, pipeline_key: String, shader_key: ShaderPermutationKey) {
        let mut map = self.pipeline_shader_map.write().unwrap();
        map.entry(pipeline_key.clone()).or_default().push(shader_key.clone());

        if let Some(perm) = self.permutations.write().unwrap().get_mut(&shader_key) {
            if !perm.pipeline_keys.contains(&pipeline_key) {
                perm.pipeline_keys.push(pipeline_key);
            }
        }
    }

    /// Mark a permutation as having successfully created a PSO.
    pub fn mark_pso_created(&self, key: &ShaderPermutationKey) {
        if let Some(perm) = self.permutations.write().unwrap().get_mut(key) {
            perm.pso_created = true;
        }
    }

    /// Get all permutations.
    pub fn all_permutations(&self) -> Vec<ShaderPermutation> {
        self.permutations.read().unwrap().values().cloned().collect()
    }

    /// Get permutations for a pipeline.
    pub fn permutations_for_pipeline(&self, pipeline_key: &str) -> Vec<ShaderPermutationKey> {
        self.pipeline_shader_map.read().unwrap()
            .get(pipeline_key).cloned().unwrap_or_default()
    }

    /// Get statistics.
    pub fn stats(&self) -> PermutationStats {
        let perms = self.permutations.read().unwrap();
        let total = perms.len();
        let with_pso = perms.values().filter(|p| p.pso_created).count();
        let total_uses: u64 = perms.values().map(|p| p.use_count).sum();
        PermutationStats {
            total_permutations: total,
            permutations_with_pso: with_pso,
            total_uses,
        }
    }

    /// Advance frame counter.
    pub fn next_frame(&self) {
        self.frame.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Export permutations for offline compilation (JSON format).
    pub fn export_for_offline_compilation(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let perms = self.all_permutations();
        let export = OfflineCompilationExport {
            version: 1,
            translation_fingerprint: victory_shader::emit::translation_fingerprint(),
            permutations: perms,
        };
        let json = serde_json::to_vec_pretty(&export)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Import permutations from offline compilation.
    pub fn import_from_offline_compilation(&self, path: &std::path::Path) -> anyhow::Result<usize> {
        let json = std::fs::read(path)?;
        let export: OfflineCompilationExport = serde_json::from_slice(&json)?;
        let mut perms = self.permutations.write().unwrap();
        let mut count = 0;
        for perm in export.permutations {
            if !perms.contains_key(&perm.key) {
                perms.insert(perm.key.clone(), perm);
                count += 1;
            }
        }
        Ok(count)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermutationStats {
    pub total_permutations: usize,
    pub permutations_with_pso: usize,
    pub total_uses: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct OfflineCompilationExport {
    version: u32,
    translation_fingerprint: [u8; 32],
    permutations: Vec<ShaderPermutation>,
}

/// Global permutation tracker instance.
static TRACKER: OnceLock<PermutationTracker> = OnceLock::new();

/// Get the global permutation tracker.
pub fn tracker() -> &'static PermutationTracker {
    TRACKER.get_or_init(PermutationTracker::new)
}

/// Enable persistence of permutation data.
pub fn enable_persistence(dir: PathBuf) {
    tracker().enable_persistence(dir);
}

/// Record a shader translation.
pub fn record_translation(
    stage: HwStage,
    container: &[u8],
    translation_fingerprint: Vec<u8>,
    root_signature_hash: [u8; 32],
    wave_mismatch: bool,
    cross_lane: bool,
    shader: &HostShader,
) -> ShaderPermutationKey {
    tracker().record_translation(stage, container, translation_fingerprint, root_signature_hash, false, false, shader)
}

/// Record a pipeline-shader relationship.
pub fn record_pipeline_shader(pipeline_key: String, shader_key: ShaderPermutationKey) {
    tracker().record_pipeline_shader(pipeline_key, shader_key);
}

/// Mark a permutation's PSO as created.
pub fn mark_pso_created(key: &ShaderPermutationKey) {
    tracker().mark_pso_created(key);
}

/// Get all permutations.
pub fn all_permutations() -> Vec<ShaderPermutation> {
    tracker().all_permutations()
}

/// Get permutation statistics.
pub fn stats() -> PermutationStats {
    tracker().stats()
}

/// Advance frame counter.
pub fn next_frame() {
    tracker().next_frame();
}

/// Export permutations for offline compilation.
pub fn export_permutations(path: &std::path::Path) -> anyhow::Result<()> {
    tracker().export_for_offline_compilation(path)
}

/// Import permutations from offline compilation.
pub fn import_permutations(path: &std::path::Path) -> anyhow::Result<usize> {
    tracker().import_from_offline_compilation(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::emit::{HostShader, StaticSampler, UserData};
    use victory_shader::{HwStage, RootType};

    fn test_shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC...".to_vec(),
            user_data: vec![UserData { index: 0, slot: 0, root: RootType::Constants, dword: 0 }],
            samplers: vec![[0; 4]],
            static_samplers: vec![StaticSampler { filter: 0, address: [0,0,0], mip_lod_bias: 0.0, max_anisotropy: 0, comparison: 0, border: 0, min_lod: 0.0, max_lod: 0.0 }],
            root_cbvs: vec![],
            root_srvs: vec![],
            root_uavs: vec![],
            root_indexed_views: vec![],
        }
    }

    #[test]
    fn permutation_key_uniqueness() {
        let fp = vec![1u8; 32];
        let rs = [2u8; 32];
        let container = b"test container";

        let key1 = ShaderPermutationKey::from_shader(HwStage::Ps, container, fp.clone(), rs, false, false);
        let key2 = ShaderPermutationKey::from_shader(HwStage::Ps, container, fp.clone(), rs, false, false);
        let key3 = ShaderPermutationKey::from_shader(HwStage::Ps, container, fp, rs, true, false); // wave mismatch

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn permutation_roundtrip() {
        let shader = test_shader();
        let mut perm = ShaderPermutation::from(&shader);
        perm.key = ShaderPermutationKey::from_shader(HwStage::Ps, b"container", vec![1u8;32], [2u8;32], false, false);
        let reconstructed = perm.to_host_shader();
        assert_eq!(reconstructed.stage, shader.stage);
        assert_eq!(reconstructed.user_data.len(), shader.user_data.len());
    }
}