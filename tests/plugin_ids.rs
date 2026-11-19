//! The plugin id interner's limit, in a process of its own: ids taken are
//! never given back, so no other test may share it.

use tinvo::clap::MAX_REFS;
use tinvo::project::Project;

#[test]
fn a_project_naming_a_plugin_id_past_the_limit_is_refused() {
    let song = |i: usize| format!("tinvo 16\ntrack kick\ninsert 0 plugin org.tinvo.ids.{i}\nend\n");
    for i in 0..MAX_REFS {
        Project::from_text(&song(i)).unwrap_or_else(|e| panic!("id {i}: {e:?}"));
    }
    let e = Project::from_text(&song(MAX_REFS)).unwrap_err();
    assert_eq!(e.line, 3);
    assert_eq!(e.message, format!("too many plugin ids in this run (at most {MAX_REFS})"));
    // The ids taken still load.
    assert!(Project::from_text(&song(0)).is_ok() && Project::from_text(&song(MAX_REFS - 1)).is_ok());
}
