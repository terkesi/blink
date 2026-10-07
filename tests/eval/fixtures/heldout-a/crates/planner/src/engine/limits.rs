pub fn soil_allowance(soil: &str) -> u32 {
    match soil { "clay" => 4, "sand" => 9, _ => 6 }
}
