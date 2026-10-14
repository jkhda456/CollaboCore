//! yq's Configured*Preferences: the settings the command line sets and the encoders read.

#[derive(Clone, Debug)]
pub struct YamlPrefs {
    pub indent: i64,
    pub colors_enabled: bool,
    pub leading_content_pre_processing: bool,
    pub print_doc_separators: bool,
    pub unwrap_scalar: bool,
    pub evaluate_together: bool,
    pub fix_merge_anchor_to_spec: bool,
    pub compact_sequence_indent: bool,
}

impl Default for YamlPrefs {
    fn default() -> Self {
        YamlPrefs {
            indent: 2,
            colors_enabled: false,
            leading_content_pre_processing: true,
            print_doc_separators: true,
            unwrap_scalar: true,
            evaluate_together: false,
            fix_merge_anchor_to_spec: false,
            compact_sequence_indent: false,
        }
    }
}

impl YamlPrefs {
    /// LoadYamlPreferences (the `load` operator's).
    pub fn load() -> YamlPrefs {
        YamlPrefs {
            indent: 0,
            colors_enabled: false,
            leading_content_pre_processing: false,
            print_doc_separators: true,
            unwrap_scalar: true,
            evaluate_together: false,
            fix_merge_anchor_to_spec: false,
            compact_sequence_indent: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct JsonPrefs {
    pub indent: i64,
    pub colors_enabled: bool,
    pub unwrap_scalar: bool,
}

impl Default for JsonPrefs {
    fn default() -> Self {
        JsonPrefs { indent: 2, colors_enabled: true, unwrap_scalar: true }
    }
}

#[derive(Clone, Debug)]
pub struct CsvPrefs {
    pub separator: char,
    pub auto_parse: bool,
}

#[derive(Clone, Debug)]
pub struct PropsPrefs {
    pub unwrap_scalar: bool,
    pub key_value_separator: String,
    pub use_array_brackets: bool,
}

impl Default for PropsPrefs {
    fn default() -> Self {
        PropsPrefs { unwrap_scalar: true, key_value_separator: " = ".into(), use_array_brackets: false }
    }
}

#[derive(Clone, Debug)]
pub struct XmlPrefs {
    pub indent: i64,
    pub attribute_prefix: String,
    pub content_name: String,
    pub strict_mode: bool,
    pub keep_namespace: bool,
    pub use_raw_token: bool,
    pub proc_inst_prefix: String,
    pub directive_name: String,
    pub skip_proc_inst: bool,
    pub skip_directives: bool,
}

impl Default for XmlPrefs {
    fn default() -> Self {
        XmlPrefs {
            indent: 2,
            attribute_prefix: "+@".into(),
            content_name: "+content".into(),
            strict_mode: false,
            keep_namespace: true,
            use_raw_token: true,
            proc_inst_prefix: "+p_".into(),
            directive_name: "+directive".into(),
            skip_proc_inst: false,
            skip_directives: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShellVarsPrefs {
    pub key_separator: String,
    pub unwrap_scalar: bool,
}

#[derive(Clone, Debug)]
pub struct LuaPrefs {
    pub doc_prefix: String,
    pub doc_suffix: String,
    pub unquoted_keys: bool,
    pub globals: bool,
}

#[derive(Clone, Debug)]
pub struct IniPrefs {
    pub colors_enabled: bool,
    pub preserve_surrounded_quote: bool,
    pub key_value_delimiters: String,
}

#[derive(Clone, Debug)]
pub struct KYamlPrefs {
    pub indent: i64,
    pub colors_enabled: bool,
    pub print_doc_separators: bool,
    pub unwrap_scalar: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SecurityPrefs {
    pub disable_env_ops: bool,
    pub disable_file_ops: bool,
    pub enable_system_ops: bool,
}

#[derive(Clone, Debug)]
pub struct Prefs {
    pub yaml: YamlPrefs,
    pub json: JsonPrefs,
    pub csv: CsvPrefs,
    pub tsv: CsvPrefs,
    pub props: PropsPrefs,
    pub xml: XmlPrefs,
    pub shell: ShellVarsPrefs,
    pub toml_colors: bool,
    pub hcl_colors: bool,
    pub lua: LuaPrefs,
    pub ini: IniPrefs,
    pub kyaml: KYamlPrefs,
    pub security: SecurityPrefs,
    pub string_interpolation: bool,
    pub debug: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            yaml: YamlPrefs::default(),
            json: JsonPrefs::default(),
            csv: CsvPrefs { separator: ',', auto_parse: true },
            tsv: CsvPrefs { separator: '\t', auto_parse: true },
            props: PropsPrefs::default(),
            xml: XmlPrefs::default(),
            shell: ShellVarsPrefs { key_separator: "_".into(), unwrap_scalar: false },
            toml_colors: false,
            hcl_colors: false,
            lua: LuaPrefs { doc_prefix: "return ".into(), doc_suffix: ";\n".into(), unquoted_keys: false, globals: false },
            ini: IniPrefs { colors_enabled: false, preserve_surrounded_quote: false, key_value_delimiters: "=:".into() },
            kyaml: KYamlPrefs { indent: 2, colors_enabled: false, print_doc_separators: true, unwrap_scalar: true },
            security: SecurityPrefs::default(),
            string_interpolation: true,
            debug: false,
        }
    }
}
