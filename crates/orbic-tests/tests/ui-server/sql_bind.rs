fn main() {
    // `owner_id` is text; an integer cannot be bound to it.
    let _ = orbic::sql!(orbic_tests::Project, "SELECT * FROM projects WHERE owner_id = $1", 7u32);
}
