//! `git lfs version`.

use super::*;
use crate::cli::{flag, K};

pub fn version_line() {
    print(&version_desc());
}

fn run(p: &Parsed) {
    version_line();
    if p.bool("comics") {
        print("Nothing may see Gah Lak Tus and survive!");
    }
}

pub fn commands() -> Vec<Cmd> {
    let mut c = cmd("version", run, vec![flag("comics", Some('c'), K::Bool)]);
    c.http_logger = false;
    vec![c]
}
