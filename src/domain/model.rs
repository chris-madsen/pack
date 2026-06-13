#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockMode {
    Raw = 0,
    Spectral = 1,
    AlphabetBreadcrumbs = 2,
    Trajectory = 3,
    Operator = 4,
    SparseAlphabet = 5,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerSummary {
    pub block_size_bytes: u32,
    pub input_size: u64,
    pub output_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveHeader {
    pub base_version: u8,
    pub block_size_bytes: u32,
    pub original_size: u64,
    pub block_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawBlock {
    pub original_len: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlphabetBlock {
    pub original_len: u32,
    pub alphabet: Vec<u8>,
    pub bit_width: u8,
    pub breadcrumbs: Vec<u8>,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpectralBlock {
    pub original_len: u32,
    pub key: Vec<u8>,
    pub residual: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrajectoryBlock {
    pub original_len: u32,
    pub key: Vec<u8>,
    pub terminals: Vec<u64>,
    pub terminal_indices: Vec<u8>,
    pub steps: u8,
    pub breadcrumbs: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorBlock {
    pub original_len: u32,
    pub key: Vec<u8>,
    pub terminals: Vec<u64>,
    pub terminal_indices: Vec<u8>,
    pub steps: u8,
    pub breadcrumbs: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockEncoding {
    Raw(RawBlock),
    Alphabet(AlphabetBlock),
    SparseAlphabet(SparseAlphabetBlock),
    Spectral(SpectralBlock),
    Trajectory(TrajectoryBlock),
    Operator(OperatorBlock),
}

impl BlockEncoding {
    pub fn mode(&self) -> BlockMode {
        match self {
            Self::Raw(_) => BlockMode::Raw,
            Self::Alphabet(_) => BlockMode::AlphabetBreadcrumbs,
            Self::SparseAlphabet(_) => BlockMode::SparseAlphabet,
            Self::Spectral(_) => BlockMode::Spectral,
            Self::Trajectory(_) => BlockMode::Trajectory,
            Self::Operator(_) => BlockMode::Operator,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Archive {
    pub header: ArchiveHeader,
    pub blocks: Vec<BlockEncoding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockAnalysis {
    pub unique_sorted_bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompressionReport {
    pub source_name: String,
    pub original_size: u64,
    pub packed_size: u64,
    pub ratio: f64,
    pub layer_count: usize,
    pub layer_summaries: Vec<LayerSummary>,
    pub roundtrip_ok: bool,
}
