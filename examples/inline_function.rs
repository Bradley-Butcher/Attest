// attest: begin
// scope: function
// id: engine.resolve_move
// module: engine
// claims:
//   - id: engine.no_test_case_heuristics
//     text: resolve_move does not add branches specific to one regression fixture.
//     review:
//       - List every branch changed inside resolve_move.
//       - Explain the domain rule behind each branch.
// attest: end
pub fn resolve_move(input: u32) -> u32 {
    input + 1
}

fn main() {
    println!("{}", resolve_move(1));
}
