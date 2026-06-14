/// A raw (uncompressed) block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawBlock {
    pub original_len: u32,
    pub payload: Vec<u8>,
}

/// Alphabet-coded block (dense).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlphabetBlock {
    pub original_len: u32,
    pub alphabet: Vec<u8>,
    pub bit_width: u8,
    pub breadcrumbs: Vec<u8>,
}

/// Alphabet-coded block (sparse exceptions).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseAlphabetBlock {
    pub original_len: u32,
    pub dense_alphabet: Vec<u8>,
    pub dense_bit_width: u8,
    pub dense_breadcrumbs: Vec<u8>,
    pub exception_alphabet: Vec<u8>,
    pub exception_indices: Vec<u8>,
    pub exception_positions: Vec<u8>,
}

/// Spectral-predictor block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpectralBlock {
    pub original_len: u32,
    pub key: Vec<u8>,
    pub residual: Vec<u8>,
}

/// Parity-trajectory block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrajectoryBlock {
    pub original_len: u32,
    pub key: Vec<u8>,
    pub terminals: Vec<u64>,
    pub terminal_indices: Vec<u8>,
    pub steps: u8,
    pub breadcrumbs: Vec<u8>,
}

/// Strict-operator (Walsh-generator) block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OperatorTerminalMode {
    RawSeed = 0,
    PaletteSeed = 1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorBlock {
    pub original_len: u32,
    pub key_bits: u16,
    pub key: Vec<u8>,
    pub steps: u8,
    pub breadcrumbs: Vec<u8>,
    pub terminal_mode: OperatorTerminalMode,
    pub terminal_payload: Vec<u8>,
}

/// Adaptive-window block (mode 0x06).
/// Wire: [0xAD tag(1)] [K u64 LE(8)] [V-len u32 LE(4)] [V(...)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdaptiveWindowBlock {
    /// Original (uncompressed) byte length of the window.
    pub original_len: u32,
    /// Packed MetaK descriptor (u64 LE).
    pub meta_k: u64,
    /// Branch-correction vector V.
    pub v: Vec<u8>,
}

/// Unified block encoding discriminant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockEncoding {
    Raw(RawBlock),
    Spectral(SpectralBlock),
    Alphabet(AlphabetBlock),
    Trajectory(TrajectoryBlock),
    Operator(OperatorBlock),
    SparseAlphabet(SparseAlphabetBlock),
    Adaptive(AdaptiveWindowBlock),
}

impl BlockEncoding {
    pub fn mode(&self) -> u8 {
        match self {
            Self::Raw(_) => 0,
            Self::Spectral(_) => 1,
            Self::Alphabet(_) => 2,
            Self::Trajectory(_) => 3,
            Self::Operator(_) => 4,
            Self::SparseAlphabet(_) => 5,
            Self::Adaptive(_) => 6,
        }
    }
}

/// Archive block record (window index + encoding).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockRecord {
    pub window_index: u8,
    pub encoding: BlockEncoding,
}

/// Block-level analysis results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockAnalysis {
    pub unique_sorted_bytes: Vec<u8>,
}

/// Window band (min/max in bits, both powers of two).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PassWindowBand {
    pub min_window_bits: u32,
    pub max_window_bits: u32,
}

/// Per-layer summary stored in a recursive archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayerSummary {
    pub window_min_bits: u32,
    pub window_max_bits: u32,
    pub input_size: u64,
    pub output_size: u64,
}

/// Archive header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveHeader {
    pub base_version: u8,
    pub window_min_exp: u8,
    pub window_max_exp: u8,
    pub original_size: u64,
    pub block_count: u32,
}

/// Full single-layer archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Archive {
    pub header: ArchiveHeader,
    pub blocks: Vec<BlockRecord>,
}
