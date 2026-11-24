use orbic_tests::Project;

fn main() {
    // Returns only some of the model's columns.
    let _ = orbic::sql!(Project, "SELECT id, title FROM projects");
    // Returns a column the model does not have.
    let _ = orbic::sql!(Project, "SELECT id, owner_id, title, is_public, version, note FROM projects");
    // A column whose type differs from the field.
    let _ = orbic::sql!(Project, "SELECT id, owner_id, title, is_public, version AS version FROM projects");
    // A nullable column into a non-`Option` field.
    let _ = orbic::sql!(Project, "SELECT id, owner_id, title, is_public, version FROM projects LEFT JOIN x");
}
