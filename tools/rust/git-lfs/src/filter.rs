//! filepathfilter: include/exclude path patterns (gitignore- or gitattributes-style).

use crate::config::cfg;
use crate::wildmatch::{Opts, Wildmatch};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PatternType {
    GitIgnore,
    GitAttributes,
}

pub struct Pattern {
    w: Option<Wildmatch>,
    p: String,
}

impl Pattern {
    pub fn new(p: &str, t: PatternType) -> Pattern {
        crate::trace!("filepathfilter: creating pattern {} of type {}", crate::tools::quote(p), if t == PatternType::GitIgnore { "gitignore" } else { "gitattributes" });
        let case_fold = cfg().git().bool("core.ignorecase", false);
        let opts = match t {
            PatternType::GitIgnore => Opts { case_fold, contents: true, ..Default::default() },
            PatternType::GitAttributes => Opts { case_fold, basename: true, gitattributes: true, ..Default::default() },
        };
        Pattern { w: Wildmatch::new(p, opts).ok(), p: p.to_string() }
    }
    pub fn matches(&self, f: &str) -> bool {
        self.w.as_ref().map_or(false, |w| w.matches(f))
    }
    pub fn as_str(&self) -> &str {
        &self.p
    }
}

pub struct Filter {
    pub include: Vec<Pattern>,
    pub exclude: Vec<Pattern>,
    pub default_value: bool,
}

impl Filter {
    pub fn new(include: &[String], exclude: &[String], t: PatternType) -> Filter {
        Filter { include: include.iter().map(|p| Pattern::new(p, t)).collect(), exclude: exclude.iter().map(|p| Pattern::new(p, t)).collect(), default_value: true }
    }
    /// NewFromPatterns with DefaultValue(false): nothing passes without an include.
    pub fn new_default_false(include: &[String], exclude: &[String], t: PatternType) -> Filter {
        Filter { default_value: false, ..Filter::new(include, exclude, t) }
    }
    pub fn include_strs(&self) -> Vec<String> {
        self.include.iter().map(|p| p.p.clone()).collect()
    }
    pub fn exclude_strs(&self) -> Vec<String> {
        self.exclude.iter().map(|p| p.p.clone()).collect()
    }
    pub fn allows(&self, f: &str) -> bool {
        let mut included = false;
        for inc in &self.include {
            if inc.matches(f) {
                included = true;
                break;
            }
        }
        if !included && !self.include.is_empty() {
            crate::trace!("filepathfilter: rejecting {} via [{}]", crate::tools::quote(f), self.include.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" "));
            return false;
        }
        if !included && !self.default_value {
            crate::trace!("filepathfilter: rejecting {}", crate::tools::quote(f));
            return false;
        }
        for ex in &self.exclude {
            if ex.matches(f) {
                crate::trace!("filepathfilter: rejecting {} via {}", crate::tools::quote(f), crate::tools::quote(ex.as_str()));
                return false;
            }
        }
        crate::trace!("filepathfilter: accepting {}", crate::tools::quote(f));
        true
    }
}
