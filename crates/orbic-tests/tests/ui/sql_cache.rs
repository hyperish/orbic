// Errors the macro reports itself, before anything is generated.
fn main() {
    let _ = orbic::sql!("SELECT 'not cached'");
    let _ = orbic::sql!("SELECT * FROM projects WHERE owner_id = $1");
    let _ = orbic::sql!("SELECT id, title FROM projects");
    let _ = orbic::sql!(42);
}
