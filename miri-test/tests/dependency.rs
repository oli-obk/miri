extern crate deptestdep;

#[test]
fn main() {
    assert_eq!(deptestdep::the_answer_to_the_ultimate_question(), 42);
}
