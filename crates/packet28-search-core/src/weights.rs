mod generated {
    include!("generated_pair_weights.rs");
}

pub use generated::WEIGHT_TABLE_VERSION;

pub fn pair_weight(left: u8, right: u8) -> u32 {
    generated::PAIR_WEIGHTS
        [((usize::from(left.to_ascii_lowercase())) << 8) | usize::from(right.to_ascii_lowercase())]
        as u32
}
