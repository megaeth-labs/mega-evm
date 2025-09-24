use revm::interpreter::gas;

fn main() {
    println!("LOGDATA: {}", gas::LOGDATA);
    println!("LOGTOPIC: {}", gas::LOGTOPIC);
    println!("STANDARD_TOKEN_COST: {}", gas::STANDARD_TOKEN_COST);
    println!("TOTAL_COST_FLOOR_PER_TOKEN: {}", gas::TOTAL_COST_FLOOR_PER_TOKEN);
}