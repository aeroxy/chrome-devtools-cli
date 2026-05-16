/// Deterministic friendly name from a target ID, e.g. "bold-fox".
/// Same target ID always produces the same name.
const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brave", "bright", "calm", "clear", "cool", "crisp", "dark", "deep", "eager",
    "fair", "fast", "firm", "fond", "free", "glad", "gold", "grand", "green", "happy", "hazy",
    "icy", "keen", "kind", "late", "lean", "light", "live", "lone", "loud", "lucky", "mild",
    "misty", "neat", "noble", "odd", "pale", "pink", "plain", "proud", "pure", "quick", "quiet",
    "rapid", "rare", "red", "rich", "rough", "royal", "rusty", "safe", "sharp", "shy", "slim",
    "slow", "snowy", "soft", "solar", "steep", "swift", "tame", "tidy", "warm",
];

const NOUNS: &[&str] = &[
    "ant", "bat", "bear", "bee", "bird", "bull", "cat", "cod", "colt", "crab", "crow", "deer",
    "dog", "dove", "duck", "eagle", "eel", "elk", "fawn", "fish", "fly", "fox", "frog", "goat",
    "hawk", "hare", "hen", "hog", "jay", "koi", "lamb", "lark", "lion", "lynx", "mole", "moth",
    "mule", "newt", "orca", "owl", "ox", "puma", "quail", "ram", "rat", "rook", "seal", "shrew",
    "slug", "snail", "snake", "squid", "stag", "swan", "toad", "trout", "vole", "wasp", "whale",
    "wolf", "wren", "yak", "zebra", "finch",
];

/// Simple hash of a string to a u64.
fn simple_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

/// Convert a target ID to a friendly "adjective-noun" name.
pub fn to_friendly(target_id: &str) -> String {
    let h = simple_hash(target_id);
    let adj = ADJECTIVES[(h % ADJECTIVES.len() as u64) as usize];
    let noun = NOUNS[((h / ADJECTIVES.len() as u64) % NOUNS.len() as u64) as usize];
    format!("{adj}-{noun}")
}

/// Check if a string looks like a friendly name (contains a hyphen, no hex chars only).
pub fn is_friendly(s: &str) -> bool {
    s.contains('-') && s.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}
