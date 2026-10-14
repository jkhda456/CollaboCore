//! yq's expression language: the participle lexer rules (first matching rule wins), yq's token
//! post-processing, the shunting-yard conversion to postfix and the expression tree.

use crate::node::{AssignPrefs, Engine, NodeId};
use regex::Regex;
use std::rc::Rc;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Or,
    And,
    Reduce,
    IfThen,
    IfElse,
    Block,
    Union,
    Pipe,
    Assign,
    AddAssign,
    SubtractAssign,
    AssignAttributes,
    AssignStyle,
    AssignVariable,
    AssignTag,
    AssignComment,
    AssignAnchor,
    AssignAlias,
    Multiply,
    MultiplyAssign,
    Divide,
    Modulo,
    Add,
    Subtract,
    Alternative,
    Equals,
    NotEquals,
    Compare,
    Min,
    Max,
    CreateMap,
    ShortPipe,
    Length,
    Line,
    Column,
    Expression,
    Collect,
    Map,
    Filter,
    Error,
    Pick,
    Omit,
    Eval,
    MapValues,
    FormatDateTime,
    WithDtFormat,
    Now,
    Tz,
    FromUnix,
    ToUnix,
    Encode,
    Decode,
    Any,
    All,
    Contains,
    AnyCondition,
    AllCondition,
    ToEntries,
    FromEntries,
    WithEntries,
    With,
    SplitDocument,
    GetVariable,
    GetStyle,
    GetTag,
    GetKind,
    GetKey,
    IsKey,
    GetParent,
    GetParents,
    GetComment,
    GetAnchor,
    GetAlias,
    GetDocumentIndex,
    GetFilename,
    GetFileIndex,
    GetPath,
    SetPath,
    DelPaths,
    Explode,
    SortBy,
    First,
    Reverse,
    Sort,
    Shuffle,
    SortKeys,
    Join,
    SubString,
    Match,
    Capture,
    Test,
    SplitString,
    ChangeCase,
    Trim,
    ToString,
    StringInterpolation,
    Load,
    LoadString,
    System,
    Keys,
    CollectObject,
    TraversePath,
    TraverseArray,
    SelfReference,
    Value,
    Reference,
    Env,
    Not,
    ToNumber,
    Empty,
    Envsubst,
    RecursiveDescent,
    Select,
    Has,
    Unique,
    UniqueBy,
    GroupBy,
    Flatten,
    DeleteChild,
    Pivot,
}

impl Op {
    /// (Type name, NumArgs, Precedence, CheckForPostTraverse)
    pub fn info(self) -> (&'static str, u32, u32, bool) {
        use Op::*;
        match self {
            Or => ("OR", 2, 20, false),
            And => ("AND", 2, 20, false),
            Reduce => ("REDUCE", 2, 35, false),
            IfThen => ("IF_THEN", 2, 5, false),
            IfElse => ("IF_ELSE", 2, 4, false),
            Block => ("BLOCK", 2, 10, false),
            Union => ("UNION", 2, 10, false),
            Pipe => ("PIPE", 2, 30, false),
            Assign => ("ASSIGN", 2, 40, false),
            AddAssign => ("ADD_ASSIGN", 2, 40, false),
            SubtractAssign => ("SUBTRACT_ASSIGN", 2, 40, false),
            AssignAttributes => ("ASSIGN_ATTRIBUTES", 2, 40, false),
            AssignStyle => ("ASSIGN_STYLE", 2, 40, false),
            AssignVariable => ("ASSIGN_VARIABLE", 2, 40, false),
            AssignTag => ("ASSIGN_TAG", 2, 40, false),
            AssignComment => ("ASSIGN_COMMENT", 2, 40, false),
            AssignAnchor => ("ASSIGN_ANCHOR", 2, 40, false),
            AssignAlias => ("ASSIGN_ALIAS", 2, 40, false),
            Multiply => ("MULTIPLY", 2, 42, false),
            MultiplyAssign => ("MULTIPLY_ASSIGN", 2, 42, false),
            Divide => ("DIVIDE", 2, 42, false),
            Modulo => ("MODULO", 2, 42, false),
            Add => ("ADD", 2, 42, false),
            Subtract => ("SUBTRACT", 2, 42, false),
            Alternative => ("ALTERNATIVE", 2, 42, false),
            Equals => ("EQUALS", 2, 40, false),
            NotEquals => ("NOT_EQUALS", 2, 40, false),
            Compare => ("COMPARE", 2, 40, false),
            Min => ("MIN", 0, 40, false),
            Max => ("MAX", 0, 40, false),
            CreateMap => ("CREATE_MAP", 2, 15, false),
            ShortPipe => ("SHORT_PIPE", 2, 45, false),
            Length => ("LENGTH", 0, 50, false),
            Line => ("LINE", 0, 50, false),
            Column => ("LINE", 0, 50, false),
            Expression => ("EXP", 0, 50, false),
            Collect => ("COLLECT", 1, 50, false),
            Map => ("MAP", 1, 52, true),
            Filter => ("FILTER", 1, 52, true),
            Error => ("ERROR", 1, 50, false),
            Pick => ("PICK", 1, 52, true),
            Omit => ("OMIT", 1, 52, true),
            Eval => ("EVAL", 1, 52, true),
            MapValues => ("MAP_VALUES", 1, 52, true),
            FormatDateTime => ("FORMAT_DATE_TIME", 1, 50, false),
            WithDtFormat => ("WITH_DATE_TIME_FORMAT", 1, 50, false),
            Now => ("NOW", 0, 50, false),
            Tz => ("TIMEZONE", 1, 50, false),
            FromUnix => ("FROM_UNIX", 0, 50, false),
            ToUnix => ("TO_UNIX", 0, 50, false),
            Encode => ("ENCODE", 0, 50, false),
            Decode => ("DECODE", 0, 50, false),
            Any => ("ANY", 0, 50, false),
            All => ("ALL", 0, 50, false),
            Contains => ("CONTAINS", 1, 50, false),
            AnyCondition => ("ANY_CONDITION", 1, 50, false),
            AllCondition => ("ALL_CONDITION", 1, 50, false),
            ToEntries => ("TO_ENTRIES", 0, 52, true),
            FromEntries => ("FROM_ENTRIES", 0, 50, false),
            WithEntries => ("WITH_ENTRIES", 1, 50, false),
            With => ("WITH", 1, 52, true),
            SplitDocument => ("SPLIT_DOC", 0, 52, true),
            GetVariable => ("GET_VARIABLE", 0, 55, false),
            GetStyle => ("GET_STYLE", 0, 50, false),
            GetTag => ("GET_TAG", 0, 50, false),
            GetKind => ("GET_KIND", 0, 50, false),
            GetKey => ("GET_KEY", 0, 50, false),
            IsKey => ("IS_KEY", 0, 50, false),
            GetParent => ("GET_PARENT", 0, 50, false),
            GetParents => ("GET_PARENTS", 0, 50, false),
            GetComment => ("GET_COMMENT", 0, 50, false),
            GetAnchor => ("GET_ANCHOR", 0, 50, false),
            GetAlias => ("GET_ALIAS", 0, 50, false),
            GetDocumentIndex => ("GET_DOCUMENT_INDEX", 0, 50, false),
            GetFilename => ("GET_FILENAME", 0, 50, false),
            GetFileIndex => ("GET_FILE_INDEX", 0, 50, false),
            GetPath => ("GET_PATH", 0, 52, true),
            SetPath => ("SET_PATH", 1, 50, false),
            DelPaths => ("DEL_PATHS", 1, 52, true),
            Explode => ("EXPLODE", 1, 52, true),
            SortBy => ("SORT_BY", 1, 52, true),
            First => ("FIRST", 1, 52, true),
            Reverse => ("REVERSE", 0, 52, true),
            Sort => ("SORT", 0, 52, true),
            Shuffle => ("SHUFFLE", 0, 52, true),
            SortKeys => ("SORT_KEYS", 1, 52, true),
            Join => ("JOIN", 1, 50, false),
            SubString => ("SUBSTR", 1, 50, false),
            Match => ("MATCH", 1, 50, false),
            Capture => ("CAPTURE", 1, 50, false),
            Test => ("TEST", 1, 50, false),
            SplitString => ("SPLIT", 1, 52, true),
            ChangeCase => ("CHANGE_CASE", 0, 50, false),
            Trim => ("TRIM", 0, 50, false),
            ToString => ("TO_STRING", 0, 50, false),
            StringInterpolation => ("STRING_INT", 0, 50, false),
            Load => ("LOAD", 1, 52, true),
            LoadString => ("LOAD_STRING", 1, 52, false),
            System => ("SYSTEM", 1, 50, false),
            Keys => ("KEYS", 0, 52, true),
            CollectObject => ("COLLECT_OBJECT", 0, 50, false),
            TraversePath => ("TRAVERSE_PATH", 0, 55, false),
            TraverseArray => ("TRAVERSE_ARRAY", 2, 50, false),
            SelfReference => ("SELF", 0, 55, false),
            Value => ("VALUE", 0, 50, false),
            Reference => ("REF", 0, 50, false),
            Env => ("ENV", 0, 52, true),
            Not => ("NOT", 0, 50, false),
            ToNumber => ("TO_NUMBER", 0, 50, false),
            Empty => ("EMPTY", 0, 50, false),
            Envsubst => ("ENVSUBST", 0, 50, false),
            RecursiveDescent => ("RECURSIVE_DESCENT", 0, 50, false),
            Select => ("SELECT", 1, 52, true),
            Has => ("HAS", 1, 50, false),
            Unique => ("UNIQUE", 0, 52, true),
            UniqueBy => ("UNIQUE_BY", 1, 52, true),
            GroupBy => ("GROUP_BY", 1, 52, true),
            Flatten => ("FLATTEN_BY", 0, 52, true),
            DeleteChild => ("DELETE", 1, 40, false),
            Pivot => ("PIVOT", 0, 52, true),
        }
    }
    pub fn name(self) -> &'static str {
        self.info().0
    }
    pub fn num_args(self) -> u32 {
        self.info().1
    }
    pub fn precedence(self) -> u32 {
        self.info().2
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum OpValue {
    None,
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TraversePrefs {
    pub dont_follow_alias: bool,
    pub include_map_keys: bool,
    pub dont_auto_create: bool,
    pub dont_include_map_values: bool,
    pub optional_traverse: bool,
    pub exact_key_match: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MultiplyPrefs {
    pub append_arrays: bool,
    pub deep_merge_arrays: bool,
    pub traverse: TraversePrefs,
    pub assign: AssignPrefs,
}

/// The formats an encode/decode/load operation uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Yaml,
    KYaml,
    Json,
    Props,
    Csv,
    Tsv,
    Xml,
    Base64,
    Base64Url,
    Uri,
    Sh,
    Toml,
    Hcl,
    Shell,
    Lua,
    Ini,
}

#[derive(Clone, Debug)]
pub enum Prefs {
    None,
    Traverse(TraversePrefs),
    Assign(AssignPrefs),
    Multiply(MultiplyPrefs),
    RecursiveDescent { traverse: TraversePrefs, recurse_array: bool },
    Comment { line: bool, head: bool, foot: bool },
    Compare { or_equal: bool, greater: bool },
    Flatten(i64),
    Parent(i64),
    Encoder { format: Fmt, indent: i64 },
    Decoder(Fmt),
    /// load with a decoder: the format (None: yaml with load preferences)
    Load(Fmt),
    ChangeCase { upper: bool },
    Env { string_value: bool, no_unset: bool, no_empty: bool, fail_fast: bool },
    AssignVar { is_reference: bool },
    Expression(&'static str),
}

#[derive(Clone, Debug)]
pub struct Operation {
    pub op: Op,
    pub value: OpValue,
    pub string_value: String,
    pub node: Option<NodeId>,
    pub prefs: Prefs,
    pub update_assign: bool,
}

impl Operation {
    pub fn new(op: Op) -> Operation {
        Operation { op, value: OpValue::None, string_value: String::new(), node: None, prefs: Prefs::None, update_assign: false }
    }
    pub fn with_prefs(op: Op, prefs: Prefs) -> Operation {
        Operation { prefs, ..Operation::new(op) }
    }
    pub fn traverse_prefs(&self) -> TraversePrefs {
        match &self.prefs {
            Prefs::Traverse(t) => *t,
            _ => TraversePrefs::default(),
        }
    }
}

#[derive(Debug)]
pub struct ExprNode {
    pub op: Operation,
    pub lhs: Option<Expr>,
    pub rhs: Option<Expr>,
}

pub type Expr = Rc<ExprNode>;

pub fn expr(op: Operation, lhs: Option<Expr>, rhs: Option<Expr>) -> Expr {
    Rc::new(ExprNode { op, lhs, rhs })
}

// ---- tokens ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TT {
    Op,
    OpenBracket,
    CloseBracket,
    OpenCollect,
    CloseCollect,
    OpenCollectObject,
    CloseCollectObject,
    TraverseArrayCollect,
}

#[derive(Clone, Debug)]
struct Token {
    tt: TT,
    op: Option<Operation>,
    assign_op: Option<Operation>,
    check_post: bool,
    match_: String,
    expected_closer: String,
    id: usize,
}

impl Token {
    fn lit(tt: TT, check_post: bool, m: &str) -> Token {
        Token { tt, op: None, assign_op: None, check_post, match_: m.into(), expected_closer: String::new(), id: 0 }
    }
    fn op(op: Operation) -> Token {
        let check = op.op.info().3;
        Token { tt: TT::Op, op: Some(op), assign_op: None, check_post: check, match_: String::new(), expected_closer: String::new(), id: 0 }
    }
    fn is_op(&self, op: Op) -> bool {
        self.tt == TT::Op && self.op.as_ref().map(|o| o.op) == Some(op)
    }
    fn to_string(&self) -> String {
        match self.tt {
            TT::Op => {
                let o = self.op.as_ref().unwrap();
                match o.op {
                    Op::TraversePath => match &o.value {
                        OpValue::Str(s) => s.clone(),
                        OpValue::Int(i) => i.to_string(),
                        _ => String::new(),
                    },
                    _ => o.op.name().to_string(),
                }
            }
            TT::OpenBracket => "(".into(),
            TT::CloseBracket => ")".into(),
            TT::OpenCollect => "[".into(),
            TT::CloseCollect => "]".into(),
            TT::OpenCollectObject => "{".into(),
            TT::CloseCollectObject => "}".into(),
            TT::TraverseArrayCollect => ".[".into(),
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    None,
    Comment(bool, bool, bool),
    Lit(TT, bool),
    LitIf,
    RecursiveDescent(bool),
    GetVariable,
    AssignVar(bool),
    Simple(Op),
    SimpleAssignable(Op, Op),
    FlattenDepth,
    Flatten,
    ExpressionOp(&'static str),
    EncodeIndent(Fmt),
    Encode(Fmt, i64),
    Decode(Fmt),
    Load(Fmt),
    ChangeCase(bool),
    Hex,
    Float,
    Number,
    Bool(bool),
    Null,
    QuotedString,
    Env(bool),
    EnvSubstOptions,
    Compare(bool, bool),
    Assign(bool),
    AllComments(bool),
    Path(bool),
    Multiply(Op),
    ParentLevel,
    ParentDefault,
}

struct Rule {
    re: Regex,
    action: Action,
}

fn rules() -> &'static Vec<Rule> {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        use Action as A;
        let defs: Vec<(&str, Action)> = vec![
            (r"line_?comment|lineComment", A::Comment(true, false, false)),
            (r"head_?comment|headComment", A::Comment(false, true, false)),
            (r"foot_?comment|footComment", A::Comment(false, false, true)),
            (r"\(", A::Lit(TT::OpenBracket, false)),
            (r"\)", A::Lit(TT::CloseBracket, true)),
            (r"\.\[", A::Lit(TT::TraverseArrayCollect, false)),
            (r"\[", A::Lit(TT::OpenCollect, false)),
            (r"\]\??", A::Lit(TT::CloseCollect, true)),
            (r"\{", A::Lit(TT::OpenCollectObject, false)),
            (r"\}", A::Lit(TT::CloseCollectObject, true)),
            (r"\.\.\.", A::RecursiveDescent(true)),
            (r"\.\.", A::RecursiveDescent(false)),
            (r"\$[a-zA-Z_\-0-9]+", A::GetVariable),
            (r"as", A::AssignVar(false)),
            (r"ref", A::AssignVar(true)),
            (r":\s*", A::Simple(Op::CreateMap)),
            ("length", A::Simple(Op::Length)),
            ("line", A::Simple(Op::Line)),
            ("column", A::Simple(Op::Column)),
            ("eval", A::Simple(Op::Eval)),
            ("to_?number", A::Simple(Op::ToNumber)),
            (r"map_?values", A::Simple(Op::MapValues)),
            ("map", A::Simple(Op::Map)),
            ("filter", A::Simple(Op::Filter)),
            ("pick", A::Simple(Op::Pick)),
            ("omit", A::Simple(Op::Omit)),
            (r"flatten\([0-9]+\)", A::FlattenDepth),
            (r"flatten", A::Flatten),
            ("format_datetime", A::Simple(Op::FormatDateTime)),
            ("now", A::Simple(Op::Now)),
            ("tz", A::Simple(Op::Tz)),
            ("from_?unix", A::Simple(Op::FromUnix)),
            ("to_?unix", A::Simple(Op::ToUnix)),
            ("with_dtf", A::Simple(Op::WithDtFormat)),
            ("error", A::Simple(Op::Error)),
            ("shuffle", A::Simple(Op::Shuffle)),
            ("sortKeys", A::Simple(Op::SortKeys)),
            ("sort_?keys", A::Simple(Op::SortKeys)),
            ("array_?to_?map", A::ExpressionOp(r#"(.[] | select(. != null) ) as $i ireduce({}; .[$i | key] = $i)"#)),
            ("root", A::ExpressionOp("parent(-1)")),
            (r"to_?yaml\([0-9]+\)", A::EncodeIndent(Fmt::Yaml)),
            (r"to_?xml\([0-9]+\)", A::EncodeIndent(Fmt::Xml)),
            (r"to_?json\([0-9]+\)", A::EncodeIndent(Fmt::Json)),
            ("from_?yaml|@yamld|from_?json|@jsond", A::Decode(Fmt::Yaml)),
            ("to_?yaml|@yaml", A::Encode(Fmt::Yaml, 2)),
            ("to_?json", A::Encode(Fmt::Json, 2)),
            ("@json", A::Encode(Fmt::Json, 0)),
            ("from_?props|@propsd", A::Decode(Fmt::Props)),
            ("to_?props|@props", A::Encode(Fmt::Props, 2)),
            ("from_?xml|@xmld", A::Decode(Fmt::Xml)),
            ("to_?xml", A::Encode(Fmt::Xml, 2)),
            ("@xml", A::Encode(Fmt::Xml, 0)),
            ("from_?csv|@csvd", A::Decode(Fmt::Csv)),
            ("to_?csv|@csv", A::Encode(Fmt::Csv, 0)),
            ("from_?tsv|@tsvd", A::Decode(Fmt::Tsv)),
            ("to_?tsv|@tsv", A::Encode(Fmt::Tsv, 0)),
            ("@base64urld", A::Decode(Fmt::Base64Url)),
            ("@base64url", A::Encode(Fmt::Base64Url, 0)),
            ("@base64d", A::Decode(Fmt::Base64)),
            ("@base64", A::Encode(Fmt::Base64, 0)),
            ("@urid", A::Decode(Fmt::Uri)),
            ("@uri", A::Encode(Fmt::Uri, 0)),
            ("@sh", A::Encode(Fmt::Sh, 0)),
            ("load_?xml|xml_?load", A::Load(Fmt::Xml)),
            ("load_?base64", A::Load(Fmt::Base64)),
            ("load_?props", A::Load(Fmt::Props)),
            ("load_?str|str_?load", A::Simple(Op::LoadString)),
            ("load", A::Load(Fmt::Yaml)),
            ("system", A::Simple(Op::System)),
            ("splitDoc|split_?doc", A::Simple(Op::SplitDocument)),
            ("select", A::Simple(Op::Select)),
            ("has", A::Simple(Op::Has)),
            ("unique_?by", A::Simple(Op::UniqueBy)),
            ("unique", A::Simple(Op::Unique)),
            ("group_?by", A::Simple(Op::GroupBy)),
            ("explode", A::Simple(Op::Explode)),
            ("or", A::Simple(Op::Or)),
            ("and", A::Simple(Op::And)),
            ("not", A::Simple(Op::Not)),
            ("ireduce", A::Simple(Op::Reduce)),
            ("join", A::Simple(Op::Join)),
            ("sub", A::Simple(Op::SubString)),
            ("match", A::Simple(Op::Match)),
            ("capture", A::Simple(Op::Capture)),
            ("test", A::Simple(Op::Test)),
            ("sort_?by", A::Simple(Op::SortBy)),
            ("sort", A::Simple(Op::Sort)),
            ("first", A::Simple(Op::First)),
            ("reverse", A::Simple(Op::Reverse)),
            ("any_c", A::Simple(Op::AnyCondition)),
            ("any", A::Simple(Op::Any)),
            ("all_c", A::Simple(Op::AllCondition)),
            ("all", A::Simple(Op::All)),
            ("contains", A::Simple(Op::Contains)),
            ("split", A::Simple(Op::SplitString)),
            ("parents", A::Simple(Op::GetParents)),
            (r"parent\(-?[0-9]+\)", A::ParentLevel),
            ("parent", A::ParentDefault),
            ("keys", A::Simple(Op::Keys)),
            ("key", A::Simple(Op::GetKey)),
            ("is_?key", A::Simple(Op::IsKey)),
            ("file_?name|fileName", A::Simple(Op::GetFilename)),
            ("file_?index|fileIndex|fi", A::Simple(Op::GetFileIndex)),
            ("path", A::Simple(Op::GetPath)),
            ("set_?path", A::Simple(Op::SetPath)),
            ("del_?paths", A::Simple(Op::DelPaths)),
            ("to_?entries|toEntries", A::Simple(Op::ToEntries)),
            ("from_?entries|fromEntries", A::Simple(Op::FromEntries)),
            ("with_?entries|withEntries", A::Simple(Op::WithEntries)),
            ("with", A::Simple(Op::With)),
            ("collect", A::Simple(Op::Collect)),
            ("del", A::Simple(Op::DeleteChild)),
            ("style", A::SimpleAssignable(Op::GetStyle, Op::AssignStyle)),
            ("tag|type", A::SimpleAssignable(Op::GetTag, Op::AssignTag)),
            ("kind", A::Simple(Op::GetKind)),
            ("anchor", A::SimpleAssignable(Op::GetAnchor, Op::AssignAnchor)),
            ("alias", A::SimpleAssignable(Op::GetAlias, Op::AssignAlias)),
            (r"comments\s*=", A::AllComments(false)),
            (r"comments\s*\|=", A::AllComments(true)),
            ("if", A::LitIf),
            ("then", A::Simple(Op::IfThen)),
            ("elif", A::Simple(Op::IfElse)),
            ("else", A::Simple(Op::IfElse)),
            ("end", A::Lit(TT::CloseBracket, true)),
            (";", A::Simple(Op::Block)),
            (r"\/\/", A::Simple(Op::Alternative)),
            ("documentIndex|document_?index|di", A::Simple(Op::GetDocumentIndex)),
            ("upcase|ascii_?upcase", A::ChangeCase(true)),
            ("downcase|ascii_?downcase", A::ChangeCase(false)),
            ("trim", A::Simple(Op::Trim)),
            ("to_?string", A::Simple(Op::ToString)),
            ("0[xX][0-9A-Fa-f]+", A::Hex),
            (r"-?[1-9](\.\d+)?[Ee][-+]?\d+", A::Float),
            (r"-?\d+(\.\d+)", A::Float),
            (r"-?\d+", A::Number),
            ("[Tt][Rr][Uu][Ee]", A::Bool(true)),
            ("[Ff][Aa][Ll][Ss][Ee]", A::Bool(false)),
            ("[Nn][Uu][Ll][Ll]|~", A::Null),
            (r#""([^"\\]*(\\.[^"\\]*)*)""#, A::QuotedString),
            (r"strenv\([^\)]+\)", A::Env(true)),
            (r"env\([^\)]+\)", A::Env(false)),
            (r"envsubst\((ne|nu|ff| |,)+\)", A::EnvSubstOptions),
            ("envsubst", A::Simple(Op::Envsubst)),
            (r"\s*==\s*", A::Simple(Op::Equals)),
            (r"\s*!=\s*", A::Simple(Op::NotEquals)),
            (r"\s*>=\s*", A::Compare(true, true)),
            (r"\s*<=\s*", A::Compare(true, false)),
            (r"\s*>\s*", A::Compare(false, true)),
            (r"\s*<\s*", A::Compare(false, false)),
            ("min", A::Simple(Op::Min)),
            ("max", A::Simple(Op::Max)),
            (r"\|=[c]*", A::Assign(true)),
            (r"=[c]*", A::Assign(false)),
            (r"[ \t\n]+", A::None),
            (r#"\."[^ "]+"\??"#, A::Path(true)),
            (r"\.[^ ;\}\{\:\[\],\|\.\[\(\)=\n!]+\??", A::Path(false)),
            (r"\|", A::Simple(Op::Pipe)),
            (r"\.", A::Simple(Op::SelfReference)),
            (",", A::Simple(Op::Union)),
            (r"\*=[\+|\?cdn]*", A::Multiply(Op::MultiplyAssign)),
            (r"\*[\+|\?cdn]*", A::Multiply(Op::Multiply)),
            (r"\/", A::Simple(Op::Divide)),
            ("%", A::Simple(Op::Modulo)),
            (r"\+=", A::Simple(Op::AddAssign)),
            (r"\+", A::Simple(Op::Add)),
            (r"\-=", A::Simple(Op::SubtractAssign)),
            (r"\-", A::Simple(Op::Subtract)),
            ("#.*", A::None),
            ("pivot", A::Simple(Op::Pivot)),
        ];
        defs.into_iter().map(|(p, action)| Rule { re: Regex::new(&format!("^(?:{p})")).unwrap(), action }).collect()
    })
}

fn extract_number_parameter(value: &str) -> Result<i64, String> {
    // `.*\((-?[0-9]+)\)`
    let open = value.rfind('(').unwrap_or(0);
    let inner = &value[open + 1..value.len() - 1];
    crate::util::parse_int(inner)
}

fn has_option_parameter(value: &str, option: &str) -> bool {
    let Some(close) = value.find(')') else { return false };
    let param = &value[..=close];
    let re = Regex::new(&format!(r"\b{option}\b")).unwrap();
    re.is_match(param)
}

/// participle's `%q` of the next 16 characters.
fn lexer_sample(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut sample: String = chars.iter().take(16).collect();
    if chars.len() > 16 {
        sample.push_str("...");
    }
    crate::util::go_quote(&sample)
}

fn tokenise(engine: &mut Engine, expression: &str) -> Result<Vec<Token>, String> {
    let rules = rules();
    let mut rest = expression;
    let (mut line, mut col) = (1usize, 1usize);
    let mut tokens = Vec::new();
    let mut next_id = 1usize;
    while !rest.is_empty() {
        let mut found: Option<(&Rule, usize)> = None;
        for r in rules.iter() {
            if let Some(m) = r.re.find(rest) {
                found = Some((r, m.end()));
                break;
            }
        }
        let Some((rule, end)) = found else {
            return Err(format!("{line}:{col}: lexer: invalid input text {}", lexer_sample(rest)));
        };
        if end == 0 {
            return Err(format!("{line}:{col}: lexer: rule did not match any input"));
        }
        let text = &rest[..end];
        let tok = make_token(engine, rule.action, text)?;
        if let Some(mut t) = tok {
            t.id = next_id;
            next_id += 1;
            tokens.push(t);
        }
        // advance the position as participle does
        let lines = text.matches('\n').count();
        if lines == 0 {
            col += text.chars().count();
        } else {
            line += lines;
            col = text[text.rfind('\n').unwrap()..].chars().count();
        }
        rest = &rest[end..];
    }
    Ok(post_process_tokens(engine, tokens))
}

fn op_with_value(op: Op, value: OpValue, s: &str) -> Operation {
    Operation { op, value, string_value: s.to_string(), node: None, prefs: Prefs::None, update_assign: false }
}

/// createValueOperation: a value op with its candidate node.
fn value_operation(engine: &mut Engine, value: OpValue, s: &str) -> Operation {
    let tag = match &value {
        OpValue::Float(_) => "!!float",
        OpValue::Int(_) => "!!int",
        OpValue::Bool(_) => "!!bool",
        OpValue::Str(_) => "!!str",
        OpValue::Null => "!!null",
        OpValue::None => "",
    };
    let node = engine.scalar(tag, s);
    Operation { op: Op::Value, value, string_value: s.to_string(), node: Some(node), prefs: Prefs::None, update_assign: false }
}

fn make_token(engine: &mut Engine, action: Action, text: &str) -> Result<Option<Token>, String> {
    use Action as A;
    let t = match action {
        A::None => return Ok(None),
        A::Comment(line, head, foot) => {
            let prefs = Prefs::Comment { line, head, foot };
            let op = Operation { prefs: prefs.clone(), string_value: text.into(), value: OpValue::Str("GET_COMMENT".into()), ..Operation::new(Op::GetComment) };
            let assign = Operation { prefs, string_value: text.into(), value: OpValue::Str("ASSIGN_COMMENT".into()), ..Operation::new(Op::AssignComment) };
            let mut t = Token::op(op);
            t.assign_op = Some(assign);
            t
        }
        A::Lit(tt, check) => Token::lit(tt, check, text),
        A::LitIf => {
            let mut t = Token::lit(TT::OpenBracket, false, text);
            t.expected_closer = "end".into();
            t
        }
        A::RecursiveDescent(include_keys) => {
            let prefs = Prefs::RecursiveDescent {
                traverse: TraversePrefs { dont_follow_alias: true, include_map_keys: include_keys, ..Default::default() },
                recurse_array: true,
            };
            Token::op(Operation { prefs, string_value: text.into(), ..Operation::new(Op::RecursiveDescent) })
        }
        A::GetVariable => {
            let name = &text[1..];
            let mut op = value_operation(engine, OpValue::Str(name.into()), name);
            op.op = Op::GetVariable;
            let mut t = Token::op(op);
            t.check_post = true;
            t
        }
        A::AssignVar(is_reference) => Token::op(Operation {
            prefs: Prefs::AssignVar { is_reference },
            string_value: text.into(),
            ..Operation::new(Op::AssignVariable)
        }),
        A::Simple(op) => Token::op(Operation { string_value: text.into(), value: OpValue::Str(op.name().into()), ..Operation::new(op) }),
        A::SimpleAssignable(get, set) => {
            let mut t = Token::op(Operation { string_value: text.into(), ..Operation::new(get) });
            t.assign_op = Some(Operation { string_value: text.into(), ..Operation::new(set) });
            t
        }
        A::FlattenDepth => {
            let depth = extract_number_parameter(text)?;
            Token::op(Operation { prefs: Prefs::Flatten(depth), string_value: text.into(), ..Operation::new(Op::Flatten) })
        }
        A::Flatten => Token::op(Operation { prefs: Prefs::Flatten(-1), string_value: text.into(), ..Operation::new(Op::Flatten) }),
        A::ExpressionOp(e) => Token::op(Operation::with_prefs(Op::Expression, Prefs::Expression(e))),
        A::EncodeIndent(format) => {
            let indent = extract_number_parameter(text)?;
            Token::op(Operation { prefs: Prefs::Encoder { format, indent }, string_value: text.into(), ..Operation::new(Op::Encode) })
        }
        A::Encode(format, indent) => {
            Token::op(Operation { prefs: Prefs::Encoder { format, indent }, string_value: text.into(), ..Operation::new(Op::Encode) })
        }
        A::Decode(format) => Token::op(Operation { prefs: Prefs::Decoder(format), string_value: text.into(), ..Operation::new(Op::Decode) }),
        A::Load(format) => Token::op(Operation { prefs: Prefs::Load(format), string_value: text.into(), ..Operation::new(Op::Load) }),
        A::ChangeCase(upper) => Token::op(Operation { prefs: Prefs::ChangeCase { upper }, string_value: text.into(), ..Operation::new(Op::ChangeCase) }),
        A::Hex => {
            let n = crate::gostr::parse_int(&text[2..], 16).map_err(|e| hex_err(&text[2..], e))?;
            Token::op(value_operation(engine, OpValue::Int(n), text))
        }
        A::Float => {
            let f = crate::util::parse_float(text)?;
            Token::op(value_operation(engine, OpValue::Float(f), text))
        }
        A::Number => {
            let n = crate::gostr::parse_int(text, 10).map_err(|e| match e {
                crate::gostr::NumErr::Range => format!("strconv.ParseInt: parsing {}: value out of range", crate::util::go_quote(text)),
                _ => format!("strconv.ParseInt: parsing {}: invalid syntax", crate::util::go_quote(text)),
            })?;
            Token::op(value_operation(engine, OpValue::Int(n), text))
        }
        A::Bool(b) => Token::op(value_operation(engine, OpValue::Bool(b), text)),
        A::Null => Token::op(value_operation(engine, OpValue::Null, text)),
        A::QuotedString => {
            let v = crate::util::process_escape_characters(&text[1..text.len() - 1]);
            Token::op(Operation { value: OpValue::Str(v.clone()), string_value: v, ..Operation::new(Op::StringInterpolation) })
        }
        A::Env(string_value) => {
            let name = if string_value { &text[7..text.len() - 1] } else { &text[4..text.len() - 1] };
            let mut op = value_operation(engine, OpValue::Str(name.into()), name);
            op.op = Op::Env;
            op.prefs = Prefs::Env { string_value, no_unset: false, no_empty: false, fail_fast: false };
            Token::op(op)
        }
        A::EnvSubstOptions => {
            let no_empty = has_option_parameter(text, "ne");
            let no_unset = has_option_parameter(text, "nu");
            let fail_fast = has_option_parameter(text, "ff");
            Token::op(Operation {
                prefs: Prefs::Env { string_value: false, no_unset, no_empty, fail_fast },
                string_value: text.into(),
                ..Operation::new(Op::Envsubst)
            })
        }
        A::Compare(or_equal, greater) => {
            Token::op(Operation { prefs: Prefs::Compare { or_equal, greater }, string_value: text.into(), ..Operation::new(Op::Compare) })
        }
        A::Assign(update) => {
            let prefs = AssignPrefs { dont_overwrite_anchor: true, clobber_custom_tags: text.contains('c'), ..Default::default() };
            Token::op(Operation { prefs: Prefs::Assign(prefs), string_value: text.into(), update_assign: update, ..Operation::new(Op::Assign) })
        }
        A::AllComments(update) => Token::op(Operation {
            prefs: Prefs::Comment { line: true, head: true, foot: true },
            string_value: text.into(),
            update_assign: update,
            ..Operation::new(Op::AssignComment)
        }),
        A::Path(wrapped) => {
            let mut value = text;
            let mut prefs = TraversePrefs::default();
            if value.ends_with('?') {
                prefs.optional_traverse = true;
                value = &value[..value.len() - 1];
            }
            value = &value[1..];
            if wrapped {
                value = &value[1..value.len() - 1];
            }
            let op = Operation {
                value: OpValue::Str(value.into()),
                string_value: value.into(),
                prefs: Prefs::Traverse(prefs),
                ..Operation::new(Op::TraversePath)
            };
            let mut t = Token::op(op);
            t.check_post = true;
            t
        }
        A::Multiply(op) => {
            let mut prefs = MultiplyPrefs::default();
            if text.contains('+') {
                prefs.append_arrays = true;
            }
            if text.contains('?') {
                prefs.traverse = TraversePrefs { dont_auto_create: true, ..Default::default() };
            }
            if text.contains('n') {
                prefs.assign.only_write_null = true;
            }
            if text.contains('d') {
                prefs.deep_merge_arrays = true;
            }
            if text.contains('c') {
                prefs.assign.clobber_custom_tags = true;
            }
            prefs.traverse.dont_follow_alias = true;
            prefs.traverse.exact_key_match = true;
            Token::op(Operation { prefs: Prefs::Multiply(prefs), string_value: text.into(), ..Operation::new(op) })
        }
        A::ParentLevel => {
            let level = extract_number_parameter(text)?;
            let mut t = Token::op(Operation { prefs: Prefs::Parent(level), string_value: text.into(), ..Operation::new(Op::GetParent) });
            t.check_post = true;
            t
        }
        A::ParentDefault => {
            let mut t = Token::op(Operation { prefs: Prefs::Parent(1), string_value: "GET_PARENT".into(), ..Operation::new(Op::GetParent) });
            t.check_post = true;
            t
        }
    };
    Ok(Some(t))
}

fn hex_err(s: &str, e: crate::gostr::NumErr) -> String {
    match e {
        crate::gostr::NumErr::Range => format!("strconv.ParseInt: parsing {}: value out of range", crate::util::go_quote(s)),
        _ => format!("strconv.ParseInt: parsing {}: invalid syntax", crate::util::go_quote(s)),
    }
}

fn post_process_tokens(engine: &mut Engine, tokens: Vec<Token>) -> Vec<Token> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for index in 0..tokens.len() {
        if skip_next {
            skip_next = false;
        } else {
            skip_next = handle_token(engine, &tokens, index, &mut out);
        }
    }
    out
}

fn handle_token(engine: &mut Engine, tokens: &[Token], index: usize, out: &mut Vec<Token>) -> bool {
    let mut skip_next = false;
    let mut current = tokens[index].clone();
    let last = index == tokens.len() - 1;
    if current.tt == TT::TraverseArrayCollect {
        out.push(Token::op(Operation { string_value: "SELF".into(), ..Operation::new(Op::SelfReference) }));
        out.push(Token::op(Operation { string_value: "TRAVERSE_ARRAY".into(), ..Operation::new(Op::TraverseArray) }));
        current = Token::lit(TT::OpenCollect, false, "");
        current.id = tokens[index].id;
    }
    if current.is_op(Op::CreateMap) {
        if index > 0 && tokens[index - 1].tt == TT::TraverseArrayCollect {
            out.push(Token::op(value_operation(engine, OpValue::Int(0), "0")));
        } else if index >= 2
            && tokens[index - 1].tt == TT::OpenCollect
            && matches!(tokens[index - 2].tt, TT::Op | TT::CloseCollect | TT::CloseCollectObject)
        {
            out.push(Token::op(value_operation(engine, OpValue::Int(0), "0")));
        }
    }
    if !last && current.assign_op.is_some() && tokens[index + 1].is_op(Op::Assign) {
        let mut a = current.assign_op.clone().unwrap();
        a.update_assign = tokens[index + 1].op.as_ref().unwrap().update_assign;
        current.op = Some(a);
        skip_next = true;
    }
    let cur_is_create_map = current.is_op(Op::CreateMap);
    let check_post = current.check_post;
    let tt = current.tt;
    out.push(current);
    if cur_is_create_map && !last && tokens[index + 1].tt == TT::CloseCollect {
        out.push(Token::op(Operation::new(Op::Length)));
    }
    if !last
        && ((tt == TT::OpenCollect && tokens[index + 1].tt == TT::CloseCollect)
            || (tt == TT::OpenCollectObject && tokens[index + 1].tt == TT::CloseCollectObject))
    {
        out.push(Token::op(Operation { string_value: "EMPTY".into(), ..Operation::new(Op::Empty) }));
    }
    if !last && check_post && (tokens[index + 1].is_op(Op::TraversePath) || tokens[index + 1].tt == TT::TraverseArrayCollect) {
        out.push(Token::op(Operation { value: OpValue::Str("PIPE".into()), string_value: ".".into(), ..Operation::new(Op::ShortPipe) }));
    }
    if !last && check_post && tokens[index + 1].tt == TT::OpenCollect {
        out.push(Token::op(Operation::new(Op::TraverseArray)));
    }
    skip_next
}

// ---- postfix ----

fn validate_no_open_tokens(t: &Token) -> Result<(), String> {
    match t.tt {
        TT::OpenCollect => Err("bad expression, could not find matching `]`".into()),
        TT::OpenCollectObject => Err("bad expression, could not find matching `}`".into()),
        TT::OpenBracket => {
            let closer = if t.expected_closer.is_empty() { ")" } else { &t.expected_closer };
            Err(format!("bad expression, could not find matching `{closer}`"))
        }
        _ => Ok(()),
    }
}

struct IfValidator {
    last_keyword: std::collections::HashMap<usize, String>,
}

impl IfValidator {
    fn on_open(&mut self, t: &Token) {
        if t.match_ == "if" {
            self.last_keyword.insert(t.id, "if".into());
        }
    }
    fn on_keyword(&mut self, op_stack: &[Token], t: &Token) -> Result<(), String> {
        if !t.is_op(Op::IfThen) && !t.is_op(Op::IfElse) {
            return Ok(());
        }
        let keyword = t.op.as_ref().unwrap().string_value.clone();
        let if_token = op_stack.iter().rev().find(|x| x.tt != TT::Op);
        let Some(if_token) = if_token.filter(|x| x.tt == TT::OpenBracket && x.match_ == "if") else {
            return Err(format!("bad expression, `{keyword}` without matching `if`"));
        };
        let previous = self.last_keyword.get(&if_token.id).cloned().unwrap_or_default();
        if keyword == "then" && previous != "if" && previous != "elif" {
            return Err("bad expression, `then` must follow `if` or `elif`".into());
        } else if keyword != "then" && previous != "then" {
            return Err(format!("bad expression, `{keyword}` must follow `then`"));
        }
        self.last_keyword.insert(if_token.id, keyword);
        Ok(())
    }
    fn on_close(&self, opener: &Token, closer: &Token) -> Result<(), String> {
        if opener.match_ != "if" && closer.match_ != "end" {
            return Ok(());
        } else if opener.match_ != "if" {
            return Err("bad expression, got `end` without matching `if`".into());
        } else if closer.match_ != "end" {
            return Err("bad expression, could not find matching `end`".into());
        }
        let previous = self.last_keyword.get(&opener.id).cloned().unwrap_or_default();
        if previous != "then" && previous != "else" {
            return Err(format!("bad expression, `{previous}` must be followed by `then`"));
        }
        Ok(())
    }
}

fn convert_to_postfix(infix: Vec<Token>) -> Result<Vec<Operation>, String> {
    let mut result: Vec<Operation> = Vec::new();
    let mut op_stack: Vec<Token> = vec![Token::lit(TT::OpenBracket, false, "")];
    let mut tokens = infix;
    tokens.push(Token::lit(TT::CloseBracket, false, ""));
    let mut ifv = IfValidator { last_keyword: Default::default() };
    fn pop(op_stack: &mut Vec<Token>, result: &mut Vec<Operation>) {
        let t = op_stack.pop().unwrap();
        result.push(t.op.unwrap_or_else(|| Operation::new(Op::Empty)));
    }
    for current in tokens {
        match current.tt {
            TT::OpenBracket | TT::OpenCollect | TT::OpenCollectObject => {
                ifv.on_open(&current);
                op_stack.push(current);
            }
            TT::CloseCollect | TT::CloseCollectObject => {
                let (opener, collect_op) =
                    if current.tt == TT::CloseCollectObject { (TT::OpenCollectObject, Op::CollectObject) } else { (TT::OpenCollect, Op::Collect) };
                while !op_stack.is_empty() && op_stack.last().unwrap().tt != opener {
                    validate_no_open_tokens(op_stack.last().unwrap())?;
                    pop(&mut op_stack, &mut result);
                }
                if op_stack.is_empty() {
                    return Err("bad path expression, got close collect brackets without matching opening bracket".into());
                }
                op_stack.pop();
                let mut prefs = TraversePrefs::default();
                if current.match_.ends_with('?') {
                    prefs.optional_traverse = true;
                }
                result.push(Operation::new(collect_op));
                if opener != TT::OpenCollect {
                    result.push(Operation::new(Op::ShortPipe));
                }
                if let Some(top) = op_stack.last_mut() {
                    if top.op.as_ref().map(|o| o.op) == Some(Op::TraverseArray) {
                        top.op.as_mut().unwrap().prefs = Prefs::Traverse(prefs);
                        pop(&mut op_stack, &mut result);
                    }
                }
            }
            TT::CloseBracket => {
                while !op_stack.is_empty() && op_stack.last().unwrap().tt != TT::OpenBracket {
                    validate_no_open_tokens(op_stack.last().unwrap())?;
                    pop(&mut op_stack, &mut result);
                }
                if op_stack.is_empty() {
                    return Err("bad expression, got close brackets without matching opening bracket".into());
                }
                ifv.on_close(op_stack.last().unwrap(), &current)?;
                op_stack.pop();
            }
            _ => {
                ifv.on_keyword(&op_stack, &current)?;
                let prec = current.op.as_ref().unwrap().op.precedence();
                while let Some(top) = op_stack.last() {
                    if top.tt == TT::Op && top.op.as_ref().unwrap().op.precedence() > prec {
                        pop(&mut op_stack, &mut result);
                    } else {
                        break;
                    }
                }
                op_stack.push(current);
            }
        }
    }
    if !op_stack.is_empty() {
        return Err(format!("bad expression - probably missing close bracket on {}", op_stack.last().unwrap().to_string()));
    }
    Ok(result)
}

fn create_expression_tree(postfix: Vec<Operation>) -> Result<Option<Expr>, String> {
    if postfix.is_empty() {
        return Ok(None);
    }
    let mut stack: Vec<Expr> = Vec::new();
    for op in postfix {
        let n = op.op.num_args();
        match n {
            1 => {
                if stack.is_empty() {
                    if op.op == Op::First {
                        stack.push(expr(op, None, None));
                        continue;
                    }
                    return Err(format!("'{}' expects 1 arg but received none", op.string_value.trim()));
                }
                let rhs = stack.pop();
                stack.push(expr(op, None, rhs));
            }
            2 => {
                if stack.len() < 2 {
                    return Err(format!("'{}' expects 2 args but there is {}", op.string_value.trim(), stack.len()));
                }
                let rhs = stack.pop();
                let lhs = stack.pop();
                stack.push(expr(op, lhs, rhs));
            }
            _ => stack.push(expr(op, None, None)),
        }
    }
    if stack.len() != 1 {
        return Err("bad expression, please check expression syntax".into());
    }
    Ok(stack.pop())
}

/// ExpressionParser.ParseExpression. None for an empty expression.
pub fn parse_expression(engine: &mut Engine, expression: &str) -> Result<Option<Expr>, String> {
    let tokens = tokenise(engine, expression)?;
    let postfix = convert_to_postfix(tokens)?;
    create_expression_tree(postfix)
}
