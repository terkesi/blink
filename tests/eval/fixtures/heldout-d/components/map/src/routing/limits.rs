pub fn slope_factor(surface: &str) -> u32 {
    match surface { "rock" => 3, "earth" => 2, _ => 1 }
}
