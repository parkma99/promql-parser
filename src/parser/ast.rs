// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::label::{Labels, Matcher, Matchers, METRIC_NAME};
use crate::parser::token::{
    self, token_display, T_BOTTOMK, T_COUNT_VALUES, T_END, T_QUANTILE, T_START, T_TOPK,
};
use crate::parser::{Function, FunctionArgs, Token, TokenId, TokenType, ValueType};
use std::ops::Neg;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// LabelModifier acts as
///
/// # Aggregation Modifier
///
/// - Exclude means `ignoring`
/// - Include means `on`
///
/// # Vector Match Modifier
///
/// - Exclude means `without` removes the listed labels from the result vector,
/// while all other labels are preserved in the output.
/// - Include means `by` does the opposite and drops labels that are not listed in the by clause,
/// even if their label values are identical between all elements of the vector.
///
/// if empty listed labels, meaning no grouping
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelModifier {
    Include(Labels),
    Exclude(Labels),
}

impl LabelModifier {
    pub fn labels(&self) -> &Labels {
        match self {
            LabelModifier::Include(l) => l,
            LabelModifier::Exclude(l) => l,
        }
    }

    /// is_on is for aggregation expr
    pub fn is_on(&self) -> bool {
        matches!(*self, LabelModifier::Include(_))
    }
}

/// The label list provided with the group_left or group_right modifier contains
/// additional labels from the "one"-side to be included in the result metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorMatchCardinality {
    OneToOne,
    ManyToOne(Labels),
    OneToMany(Labels),
    ManyToMany, // logical/set binary operators
}

impl VectorMatchCardinality {
    pub fn labels(&self) -> Option<&Labels> {
        match self {
            VectorMatchCardinality::ManyToOne(l) => Some(l),
            VectorMatchCardinality::OneToMany(l) => Some(l),
            VectorMatchCardinality::ManyToMany => None,
            VectorMatchCardinality::OneToOne => None,
        }
    }
}

/// Binary Expr Modifier
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinModifier {
    /// The matching behavior for the operation if both operands are Vectors.
    /// If they are not this field is None.
    pub card: VectorMatchCardinality,

    /// on/ignoring on labels.
    /// like a + b, no match modifier is needed.
    pub matching: Option<LabelModifier>,
    /// If a comparison operator, return 0/1 rather than filtering.
    pub return_bool: bool,
}

impl Default for BinModifier {
    fn default() -> Self {
        Self {
            card: VectorMatchCardinality::OneToOne,
            matching: None,
            return_bool: false,
        }
    }
}

impl BinModifier {
    pub fn with_card(mut self, card: VectorMatchCardinality) -> Self {
        self.card = card;
        self
    }

    pub fn with_matching(mut self, matching: Option<LabelModifier>) -> Self {
        self.matching = matching;
        self
    }

    pub fn with_return_bool(mut self, return_bool: bool) -> Self {
        self.return_bool = return_bool;
        self
    }

    pub fn is_labels_joint(&self) -> bool {
        matches!((self.card.labels(), &self.matching),
                 (Some(labels), Some(matching)) if !matching.labels().is_disjoint(labels))
    }

    pub fn intersect_labels(&self) -> Option<Vec<&String>> {
        if let Some(labels) = self.card.labels() {
            if let Some(matching) = &self.matching {
                return Some(matching.labels().intersection(labels).collect());
            }
        };
        None
    }

    pub fn is_matching_on(&self) -> bool {
        matches!(&self.matching, Some(matching) if matching.is_on())
    }

    pub fn is_matching_labels_not_empty(&self) -> bool {
        matches!(&self.matching, Some(matching) if !matching.labels().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offset {
    Pos(Duration),
    Neg(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtModifier {
    Start,
    End,
    /// at can be earlier than UNIX_EPOCH
    At(SystemTime),
}

impl TryFrom<TokenId> for AtModifier {
    type Error = String;

    fn try_from(id: TokenId) -> Result<Self, Self::Error> {
        match id {
            T_START => Ok(AtModifier::Start),
            T_END => Ok(AtModifier::End),
            _ => Err(format!(
                "invalid @ modifier preprocessor '{}', START or END is valid.",
                token::token_display(id)
            )),
        }
    }
}

impl TryFrom<Token> for AtModifier {
    type Error = String;

    fn try_from(token: Token) -> Result<Self, Self::Error> {
        AtModifier::try_from(token.id())
    }
}

impl TryFrom<NumberLiteral> for AtModifier {
    type Error = String;

    fn try_from(num: NumberLiteral) -> Result<Self, Self::Error> {
        AtModifier::try_from(num.val)
    }
}

impl TryFrom<Expr> for AtModifier {
    type Error = String;

    fn try_from(ex: Expr) -> Result<Self, Self::Error> {
        match ex {
            Expr::NumberLiteral(nl) => AtModifier::try_from(nl),
            _ => Err("invalid float value after @ modifier".into()),
        }
    }
}

impl TryFrom<f64> for AtModifier {
    type Error = String;

    fn try_from(secs: f64) -> Result<Self, Self::Error> {
        let err_info = format!("timestamp out of bounds for @ modifier: {secs}");

        if secs.is_nan() || secs.is_infinite() || secs >= f64::MAX || secs <= f64::MIN {
            return Err(err_info);
        }
        let milli = (secs * 1000f64).round().abs() as u64;

        let duration = Duration::from_millis(milli);
        let mut st = Some(SystemTime::UNIX_EPOCH);
        if secs.is_sign_positive() {
            st = SystemTime::UNIX_EPOCH.checked_add(duration);
        }
        if secs.is_sign_negative() {
            st = SystemTime::UNIX_EPOCH.checked_sub(duration);
        }

        st.map(Self::At).ok_or(err_info)
    }
}

/// EvalStmt holds an expression and information on the range it should
/// be evaluated on.
#[allow(rustdoc::broken_intra_doc_links)]
#[derive(Debug, Clone)]
pub struct EvalStmt {
    /// Expression to be evaluated.
    pub expr: Expr,

    /// The time boundaries for the evaluation. If start equals end an instant
    /// is evaluated.
    pub start: SystemTime,
    pub end: SystemTime,
    /// Time between two evaluated instants for the range [start:end].
    pub interval: Duration,
    /// Lookback delta to use for this evaluation.
    pub lookback_delta: Duration,
}

/// Grammar:
/// ``` norust
/// <aggr-op> [without|by (<label list>)] ([parameter,] <vector expression>)
/// <aggr-op>([parameter,] <vector expression>) [without|by (<label list>)]
/// ```
///
/// parameter is only required for `count_values`, `quantile`, `topk` and `bottomk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateExpr {
    /// The used aggregation operation.
    pub op: TokenType,
    /// The Vector expression over which is aggregated.
    pub expr: Box<Expr>,
    /// Parameter used by some aggregators.
    pub param: Option<Box<Expr>>,
    /// modifier is optional for some aggregation operators, like sum.
    pub modifier: Option<LabelModifier>,
}

/// UnaryExpr will negate the expr
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnaryExpr {
    pub expr: Box<Expr>,
}

/// Grammar:
/// ``` norust
/// <vector expr> <bin-op> ignoring(<label list>) group_left(<label list>) <vector expr>
/// <vector expr> <bin-op> ignoring(<label list>) group_right(<label list>) <vector expr>
/// <vector expr> <bin-op> on(<label list>) group_left(<label list>) <vector expr>
/// <vector expr> <bin-op> on(<label list>) group_right(<label list>) <vector expr>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryExpr {
    /// The operation of the expression.
    pub op: TokenType,
    /// The operands on the left sides of the operator.
    pub lhs: Box<Expr>,
    /// The operands on the right sides of the operator.
    pub rhs: Box<Expr>,

    pub modifier: Option<BinModifier>,
}

impl BinaryExpr {
    pub fn is_matching_on(&self) -> bool {
        matches!(&self.modifier, Some(modifier) if modifier.is_matching_on())
    }

    pub fn is_matching_labels_not_empty(&self) -> bool {
        matches!(&self.modifier, Some(modifier) if modifier.is_matching_labels_not_empty())
    }

    pub fn return_bool(&self) -> bool {
        matches!(&self.modifier, Some(modifier) if modifier.return_bool)
    }

    /// check if labels of card and matching are joint
    pub fn is_labels_joint(&self) -> bool {
        matches!(&self.modifier, Some(modifier) if modifier.is_labels_joint())
    }

    /// intersect labels of card and matching
    pub fn intersect_labels(&self) -> Option<Vec<&String>> {
        self.modifier
            .as_ref()
            .and_then(|modifier| modifier.intersect_labels())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParenExpr {
    pub expr: Box<Expr>,
}

/// Grammar:
/// ```norust
/// <instant_query> '[' <range> ':' [<resolution>] ']' [ @ <float_literal> ] [ offset <duration> ]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubqueryExpr {
    pub expr: Box<Expr>,
    pub offset: Option<Offset>,
    pub at: Option<AtModifier>,
    pub range: Duration,
    /// Default is the global evaluation interval.
    pub step: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct NumberLiteral {
    pub val: f64,
}

impl NumberLiteral {
    pub fn new(val: f64) -> Self {
        Self { val }
    }
}

impl PartialEq for NumberLiteral {
    fn eq(&self, other: &Self) -> bool {
        self.val == other.val || self.val.is_nan() && other.val.is_nan()
    }
}

impl Eq for NumberLiteral {}

impl Neg for NumberLiteral {
    type Output = Self;

    fn neg(self) -> Self::Output {
        NumberLiteral { val: -self.val }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteral {
    pub val: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorSelector {
    pub name: Option<String>,
    pub matchers: Matchers,
    pub offset: Option<Offset>,
    pub at: Option<AtModifier>,
}

impl From<String> for VectorSelector {
    fn from(name: String) -> Self {
        let matcher = Matcher::new_eq_metric_matcher(name.clone());
        VectorSelector {
            name: Some(name),
            offset: None,
            at: None,
            matchers: Matchers::one(matcher),
        }
    }
}

/// directly create an instant vector with only METRIC_NAME matcher.
///
/// # Examples
///
/// Basic usage:
///
/// ``` rust
/// use promql_parser::parser::{Expr, VectorSelector};
/// use promql_parser::label::{MatchOp, Matcher, Matchers};
///
/// let matcher = Matcher::new_eq_metric_matcher(String::from("foo"));
/// let vs = VectorSelector {
///     name: Some(String::from("foo")),
///     offset: None,
///     at: None,
///     matchers: Matchers::one(matcher),
/// };
///
/// assert_eq!(VectorSelector::from("foo"), vs);
/// ```
impl From<&str> for VectorSelector {
    fn from(name: &str) -> Self {
        VectorSelector::from(name.to_string())
    }
}

impl Neg for VectorSelector {
    type Output = UnaryExpr;

    fn neg(self) -> Self::Output {
        let ex = Expr::VectorSelector(self);
        UnaryExpr { expr: Box::new(ex) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixSelector {
    pub vector_selector: VectorSelector,
    pub range: Duration,
}

/// Call represents Prometheus Function.
/// Some functions have special cases:
///
/// ## exp
///
/// exp(v instant-vector) calculates the exponential function for all elements in v.
/// Special cases are:
///
/// ```promql
/// Exp(+Inf) = +Inf
/// Exp(NaN) = NaN
/// ```
///
/// ## ln
///
/// ln(v instant-vector) calculates the natural logarithm for all elements in v.
/// Special cases are:
///
/// ```promql
/// ln(+Inf) = +Inf
/// ln(0) = -Inf
/// ln(x < 0) = NaN
/// ln(NaN) = NaN
/// ```
///
/// TODO: support more special cases of function call
///
///  - acos()
///  - acosh()
///  - asin()
///  - asinh()
///  - atan()
///  - atanh()
///  - cos()
///  - cosh()
///  - sin()
///  - sinh()
///  - tan()
///  - tanh()
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub func: Function,
    pub args: FunctionArgs,
}

/// Node for extending the AST. [Extension] won't be generate by this parser itself.
#[derive(Debug, Clone)]
pub struct Extension {
    pub expr: Arc<dyn ExtensionExpr>,
}

/// The interface for extending the AST with custom expression node.
pub trait ExtensionExpr: std::fmt::Debug + Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;

    fn name(&self) -> &str;

    fn value_type(&self) -> ValueType;

    fn children(&self) -> &[Expr];
}

impl PartialEq for Extension {
    fn eq(&self, other: &Self) -> bool {
        format!("{:?}", self) == format!("{:?}", other)
    }
}

impl Eq for Extension {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Aggregate represents an aggregation operation on a Vector.
    Aggregate(AggregateExpr),

    /// Unary represents a unary operation on another expression.
    /// Currently unary operations are only supported for Scalars.
    Unary(UnaryExpr),

    /// Binary represents a binary expression between two child expressions.
    Binary(BinaryExpr),

    /// Paren wraps an expression so it cannot be disassembled as a consequence
    /// of operator precedence.
    Paren(ParenExpr),

    /// SubqueryExpr represents a subquery.
    Subquery(SubqueryExpr),

    /// NumberLiteral represents a number.
    NumberLiteral(NumberLiteral),

    /// StringLiteral represents a string.
    StringLiteral(StringLiteral),

    /// VectorSelector represents a Vector selection.
    VectorSelector(VectorSelector),

    /// MatrixSelector represents a Matrix selection.
    MatrixSelector(MatrixSelector),

    /// Call represents a function call.
    Call(Call),

    /// Extension represents an extension expression. It is for user to attach additional
    /// informations to the AST. This parser won't generate Extension node.
    Extension(Extension),
}

impl Expr {
    pub fn new_vector_selector(name: Option<String>, matchers: Matchers) -> Result<Self, String> {
        let vs = VectorSelector {
            name,
            offset: None,
            at: None,
            matchers,
        };
        Ok(Self::VectorSelector(vs))
    }

    pub fn new_unary_expr(expr: Expr) -> Result<Self, String> {
        match expr {
            Expr::StringLiteral(_) => Err("unary expression only allowed on expressions of type scalar or vector, got: string".into()),
            Expr::MatrixSelector(_) => Err("unary expression only allowed on expressions of type scalar or vector, got: matrix".into()),
            _ => Ok(-expr),
        }
    }

    pub fn new_subquery_expr(
        expr: Expr,
        range: Duration,
        step: Option<Duration>,
    ) -> Result<Self, String> {
        let se = Expr::Subquery(SubqueryExpr {
            expr: Box::new(expr),
            offset: None,
            at: None,
            range,
            step,
        });
        Ok(se)
    }

    pub fn new_paren_expr(expr: Expr) -> Result<Self, String> {
        let ex = Expr::Paren(ParenExpr {
            expr: Box::new(expr),
        });
        Ok(ex)
    }

    /// NOTE: @ and offset is not set here.
    pub fn new_matrix_selector(expr: Expr, range: Duration) -> Result<Self, String> {
        match expr {
            Expr::VectorSelector(VectorSelector {
                offset: Some(_), ..
            }) => Err("no offset modifiers allowed before range".into()),
            Expr::VectorSelector(VectorSelector { at: Some(_), .. }) => {
                Err("no @ modifiers allowed before range".into())
            }
            Expr::VectorSelector(vs) => {
                let ms = Expr::MatrixSelector(MatrixSelector {
                    vector_selector: vs,
                    range,
                });
                Ok(ms)
            }
            _ => Err("ranges only allowed for vector selectors".into()),
        }
    }

    pub fn at_expr(self, at: AtModifier) -> Result<Self, String> {
        let already_set_err = Err("@ <timestamp> may not be set multiple times".into());
        match self {
            Expr::VectorSelector(mut vs) => match vs.at {
                None => {
                    vs.at = Some(at);
                    Ok(Expr::VectorSelector(vs))
                }
                Some(_) => already_set_err,
            },
            Expr::MatrixSelector(mut ms) => match ms.vector_selector.at {
                None => {
                    ms.vector_selector.at = Some(at);
                    Ok(Expr::MatrixSelector(ms))
                }
                Some(_) => already_set_err,
            },
            Expr::Subquery(mut s) => match s.at {
                None => {
                    s.at = Some(at);
                    Ok(Expr::Subquery(s))
                }
                Some(_) => already_set_err,
            },
            _ => {
                Err("@ modifier must be preceded by an vector selector or matrix selector or a subquery".into())
            }
        }
    }

    /// set offset field for specified Expr, but CAN ONLY be set once.
    pub fn offset_expr(self, offset: Offset) -> Result<Self, String> {
        let already_set_err = Err("offset may not be set multiple times".into());
        match self {
            Expr::VectorSelector(mut vs) => match vs.offset {
                None => {
                    vs.offset = Some(offset);
                    Ok(Expr::VectorSelector(vs))
                }
                Some(_) => already_set_err,
            },
            Expr::MatrixSelector(mut ms) => match ms.vector_selector.offset {
                None => {
                    ms.vector_selector.offset = Some(offset);
                    Ok(Expr::MatrixSelector(ms))
                }
                Some(_) => already_set_err,
            },
            Expr::Subquery(mut s) => match s.offset {
                None => {
                    s.offset = Some(offset);
                    Ok(Expr::Subquery(s))
                }
                Some(_) => already_set_err,
            },
            _ => {
                Err("offset modifier must be preceded by an vector selector or matrix selector or a subquery".into())
            }
        }
    }

    pub fn new_call(func: Function, args: FunctionArgs) -> Result<Expr, String> {
        Ok(Expr::Call(Call { func, args }))
    }

    pub fn new_binary_expr(
        lhs: Expr,
        op: TokenId,
        modifier: Option<BinModifier>,
        rhs: Expr,
    ) -> Result<Expr, String> {
        let ex = BinaryExpr {
            op: TokenType::new(op),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            modifier,
        };
        Ok(Expr::Binary(ex))
    }

    pub fn new_aggregate_expr(
        op: TokenId,
        modifier: Option<LabelModifier>,
        args: FunctionArgs,
    ) -> Result<Expr, String> {
        let op = TokenType::new(op);
        if args.is_empty() {
            let op_display = token_display(op.id());
            return Err(format!(
                "no arguments for aggregate expression '{op_display}' provided"
            ));
        }
        let mut desired_args_count = 1;
        let mut param = None;
        if op.is_aggregator_with_param() {
            desired_args_count = 2;
            param = args.first();
        }
        if args.len() != desired_args_count {
            return Err(format!(
                "wrong number of arguments for aggregate expression provided, expected {}, got {}",
                desired_args_count,
                args.len()
            ));
        }

        match args.last() {
            Some(expr) => Ok(Expr::Aggregate(AggregateExpr {
                op,
                expr,
                param,
                modifier,
            })),
            None => Err(
                "aggregate operation needs a single instant vector parameter, but found none"
                    .into(),
            ),
        }
    }

    pub fn value_type(&self) -> ValueType {
        match self {
            Expr::Aggregate(_) => ValueType::Vector,
            Expr::Unary(ex) => ex.expr.value_type(),
            Expr::Binary(ex) => {
                if ex.lhs.value_type() == ValueType::Scalar
                    && ex.rhs.value_type() == ValueType::Scalar
                {
                    ValueType::Scalar
                } else {
                    ValueType::Vector
                }
            }
            Expr::Paren(ex) => ex.expr.value_type(),
            Expr::Subquery(_) => ValueType::Matrix,
            Expr::NumberLiteral(_) => ValueType::Scalar,
            Expr::StringLiteral(_) => ValueType::String,
            Expr::VectorSelector(_) => ValueType::Vector,
            Expr::MatrixSelector(_) => ValueType::Matrix,
            Expr::Call(ex) => ex.func.return_type,
            Expr::Extension(ex) => ex.expr.value_type(),
        }
    }

    /// only Some if expr is [Expr::NumberLiteral]
    pub fn scalar_value(&self) -> Option<f64> {
        match self {
            Expr::NumberLiteral(nl) => Some(nl.val),
            _ => None,
        }
    }
}

impl From<String> for Expr {
    fn from(val: String) -> Self {
        Expr::StringLiteral(StringLiteral { val })
    }
}

impl From<&str> for Expr {
    fn from(s: &str) -> Self {
        Expr::StringLiteral(StringLiteral { val: s.into() })
    }
}

impl From<f64> for Expr {
    fn from(val: f64) -> Self {
        Expr::NumberLiteral(NumberLiteral { val })
    }
}

/// directly create an Expr::VectorSelector from instant vector
///
/// # Examples
///
/// Basic usage:
///
/// ``` rust
/// use promql_parser::parser::{Expr, VectorSelector};
/// use promql_parser::label::{MatchOp, Matcher, Matchers};
///
/// let name = String::from("foo");
/// let matcher = Matcher::new_eq_metric_matcher(name.clone());
/// let vs = Expr::new_vector_selector(Some(name), Matchers::one(matcher));
///
/// assert_eq!(Expr::from(VectorSelector::from("foo")), vs.unwrap());
/// ```
impl From<VectorSelector> for Expr {
    fn from(vs: VectorSelector) -> Self {
        Expr::VectorSelector(vs)
    }
}

impl Neg for Expr {
    type Output = Self;

    fn neg(self) -> Self::Output {
        match self {
            Expr::NumberLiteral(nl) => Expr::NumberLiteral(-nl),
            _ => Expr::Unary(UnaryExpr {
                expr: Box::new(self),
            }),
        }
    }
}

/// check_ast checks the validity of the provided AST. This includes type checking.
/// Recursively check correct typing for child nodes and raise errors in case of bad typing.
pub fn check_ast(expr: Expr) -> Result<Expr, String> {
    match expr {
        Expr::Binary(ex) => check_ast_for_binary_expr(ex),
        Expr::Aggregate(ex) => check_ast_for_aggregate_expr(ex),
        Expr::Call(ex) => check_ast_for_call(ex),
        Expr::Unary(ex) => check_ast_for_unary(ex),
        Expr::Subquery(ex) => check_ast_for_subquery(ex),
        Expr::VectorSelector(ex) => check_ast_for_vector_selector(ex),
        Expr::Paren(_) => Ok(expr),
        Expr::NumberLiteral(_) => Ok(expr),
        Expr::StringLiteral(_) => Ok(expr),
        Expr::MatrixSelector(_) => Ok(expr),
        Expr::Extension(_) => Ok(expr),
    }
}

fn expect_type(
    expected: ValueType,
    actual: Option<ValueType>,
    context: &str,
) -> Result<bool, String> {
    match actual {
        Some(actual) => {
            if actual == expected {
                Ok(true)
            } else {
                Err(format!(
                    "expected type {expected} in {context}, got {actual}"
                ))
            }
        }
        None => Err(format!("expected type {expected} in {context}, got None")),
    }
}

/// the original logic is redundant in prometheus, and the following coding blocks
/// have been optimized for readability, but all logic SHOULD be covered.
fn check_ast_for_binary_expr(mut ex: BinaryExpr) -> Result<Expr, String> {
    let op_display = token_display(ex.op.id());

    if !ex.op.is_operator() {
        return Err(format!(
            "binary expression does not support operator '{op_display}'"
        ));
    }

    if ex.return_bool() && !ex.op.is_comparison_operator() {
        return Err("bool modifier can only be used on comparison operators".into());
    }

    if ex.op.is_comparison_operator()
        && ex.lhs.value_type() == ValueType::Scalar
        && ex.rhs.value_type() == ValueType::Scalar
        && !ex.return_bool()
    {
        return Err("comparisons between scalars must use BOOL modifier".into());
    }

    // For `on` matching, a label can only appear in one of the lists.
    // Every time series of the result vector must be uniquely identifiable.
    if ex.is_matching_on() && ex.is_labels_joint() {
        if let Some(labels) = ex.intersect_labels() {
            if let Some(label) = labels.first() {
                return Err(format!(
                    "label '{label}' must not occur in ON and GROUP clause at once"
                ));
            }
        };
    }

    if ex.op.is_set_operator() {
        if ex.lhs.value_type() == ValueType::Scalar || ex.rhs.value_type() == ValueType::Scalar {
            return Err(format!(
                "set operator '{op_display}' not allowed in binary scalar expression"
            ));
        }

        if ex.lhs.value_type() == ValueType::Vector && ex.rhs.value_type() == ValueType::Vector {
            if let Some(ref modifier) = ex.modifier {
                if matches!(modifier.card, VectorMatchCardinality::OneToMany(_))
                    || matches!(modifier.card, VectorMatchCardinality::ManyToOne(_))
                {
                    return Err(format!("no grouping allowed for '{op_display}' operation"));
                }
            };
        }

        match &mut ex.modifier {
            Some(modifier) => {
                if modifier.card == VectorMatchCardinality::OneToOne {
                    modifier.card = VectorMatchCardinality::ManyToMany;
                }
            }
            None => {
                ex.modifier =
                    Some(BinModifier::default().with_card(VectorMatchCardinality::ManyToMany));
            }
        }
    }

    if ex.lhs.value_type() != ValueType::Scalar && ex.lhs.value_type() != ValueType::Vector {
        return Err("binary expression must contain only scalar and instant vector types".into());
    }
    if ex.rhs.value_type() != ValueType::Scalar && ex.rhs.value_type() != ValueType::Vector {
        return Err("binary expression must contain only scalar and instant vector types".into());
    }

    if (ex.lhs.value_type() != ValueType::Vector || ex.rhs.value_type() != ValueType::Vector)
        && ex.is_matching_labels_not_empty()
    {
        return Err("vector matching only allowed between vectors".into());
    }

    Ok(Expr::Binary(ex))
}

fn check_ast_for_aggregate_expr(ex: AggregateExpr) -> Result<Expr, String> {
    if !ex.op.is_aggregator() {
        let op_display = token_display(ex.op.id());
        return Err(format!(
            "aggregation operator expected in aggregation expression but got '{op_display}'"
        ));
    }

    expect_type(
        ValueType::Vector,
        Some(ex.expr.value_type()),
        "aggregation expression",
    )?;

    if matches!(ex.op.id(), T_TOPK | T_BOTTOMK | T_QUANTILE) {
        expect_type(
            ValueType::Scalar,
            ex.param.as_ref().map(|ex| ex.value_type()),
            "aggregation expression",
        )?;
    }

    if ex.op.id() == T_COUNT_VALUES {
        expect_type(
            ValueType::String,
            ex.param.as_ref().map(|ex| ex.value_type()),
            "aggregation expression",
        )?;
    }

    Ok(Expr::Aggregate(ex))
}

fn check_ast_for_call(ex: Call) -> Result<Expr, String> {
    let expected_args_len = ex.func.arg_types.len();
    let name = ex.func.name;
    let actual_args_len = ex.args.len();

    if ex.func.variadic {
        let expected_args_len_without_default = expected_args_len - 1;
        if expected_args_len_without_default > actual_args_len {
            return Err(format!(
                "expected at least {expected_args_len_without_default} argument(s) in call to '{name}', got {actual_args_len}"
            ));
        }

        // `label_join` do not have a maximum arguments threshold.
        // this hard code SHOULD be careful if new functions are supported by Prometheus.
        if actual_args_len > expected_args_len && name.ne("label_join") {
            return Err(format!(
                "expected at most {expected_args_len} argument(s) in call to '{name}', got {actual_args_len}"
            ));
        }
    }

    if !ex.func.variadic && expected_args_len != actual_args_len {
        return Err(format!(
            "expected {expected_args_len} argument(s) in call to '{name}', got {actual_args_len}"
        ));
    }

    // special cases from https://prometheus.io/docs/prometheus/latest/querying/functions
    if name.eq_ignore_ascii_case("exp") {
        if let Some(val) = ex.args.first().and_then(|ex| ex.scalar_value()) {
            if val.is_nan() || val.is_infinite() {
                return Ok(Expr::Call(ex));
            }
        }
    } else if name.eq_ignore_ascii_case("ln")
        || name.eq_ignore_ascii_case("log2")
        || name.eq_ignore_ascii_case("log10")
    {
        if let Some(val) = ex.args.first().and_then(|ex| ex.scalar_value()) {
            if val.is_nan() || val.is_infinite() || val <= 0.0 {
                return Ok(Expr::Call(ex));
            }
        }
    }

    for (mut idx, actual_arg) in ex.args.args.iter().enumerate() {
        // this only happens when function args are variadic
        if idx >= ex.func.arg_types.len() {
            idx = ex.func.arg_types.len() - 1;
        }

        expect_type(
            ex.func.arg_types[idx],
            Some(actual_arg.value_type()),
            &format!("call to function '{name}'"),
        )?;
    }

    Ok(Expr::Call(ex))
}

fn check_ast_for_unary(ex: UnaryExpr) -> Result<Expr, String> {
    let value_type = ex.expr.value_type();
    if value_type != ValueType::Scalar && value_type != ValueType::Vector {
        return Err(format!(
            "unary expression only allowed on expressions of type scalar or vector, got {value_type}"
        ));
    }

    Ok(Expr::Unary(ex))
}

fn check_ast_for_subquery(ex: SubqueryExpr) -> Result<Expr, String> {
    let value_type = ex.expr.value_type();
    if value_type != ValueType::Vector {
        return Err(format!(
            "subquery is only allowed on vector, got {value_type} instead"
        ));
    }

    Ok(Expr::Subquery(ex))
}

fn check_ast_for_vector_selector(ex: VectorSelector) -> Result<Expr, String> {
    // A Vector selector must contain at least one non-empty matcher to prevent
    // implicit selection of all metrics (e.g. by a typo).
    if ex.matchers.is_empty_matchers() {
        return Err("vector selector must contain at least one non-empty matcher".into());
    }

    let mut du = ex.matchers.find_matchers(METRIC_NAME);
    if du.len() >= 2 {
        // this is to ensure that the err information can be predicted with fixed order
        du.sort();
        return Err(format!(
            "metric name must not be set twice: '{}' or '{}'",
            du[0], du[1]
        ));
    }

    Ok(Expr::VectorSelector(ex))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn test_valid_at_modifier() {
        let cases = vec![
            // tuple: (seconds, elapsed milliseconds before or after UNIX_EPOCH)
            (0.0, 0),
            (1000.3, 1000300),    // after UNIX_EPOCH
            (1000.9, 1000900),    // after UNIX_EPOCH
            (1000.9991, 1000999), // after UNIX_EPOCH
            (1000.9999, 1001000), // after UNIX_EPOCH
            (-1000.3, 1000300),   // before UNIX_EPOCH
            (-1000.9, 1000900),   // before UNIX_EPOCH
        ];

        for (secs, elapsed) in cases {
            match AtModifier::try_from(secs).unwrap() {
                AtModifier::At(st) => {
                    if secs.is_sign_positive() || secs == 0.0 {
                        assert_eq!(
                            elapsed,
                            st.duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap()
                                .as_millis()
                        )
                    } else if secs.is_sign_negative() {
                        assert_eq!(
                            elapsed,
                            SystemTime::UNIX_EPOCH
                                .duration_since(st)
                                .unwrap()
                                .as_millis()
                        )
                    }
                }
                _ => panic!(),
            }
        }

        assert_eq!(
            AtModifier::try_from(Expr::from(1.0)),
            AtModifier::try_from(1.0),
        );
    }

    #[test]
    fn test_invalid_at_modifier() {
        let cases = vec![
            f64::MAX,
            f64::MIN,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];

        for secs in cases {
            assert!(AtModifier::try_from(secs).is_err())
        }

        assert_eq!(
            AtModifier::try_from(token::T_ADD),
            Err("invalid @ modifier preprocessor '+', START or END is valid.".into())
        );

        assert_eq!(
            AtModifier::try_from(Expr::from("string literal")),
            Err("invalid float value after @ modifier".into())
        );
    }

    #[test]
    fn test_binary_labels() {
        assert_eq!(
            LabelModifier::Include(HashSet::from([String::from("foo"), String::from("bar")]))
                .labels(),
            &HashSet::from([String::from("foo"), String::from("bar")])
        );

        assert_eq!(
            LabelModifier::Exclude(HashSet::from([String::from("foo"), String::from("bar")]))
                .labels(),
            &HashSet::from([String::from("foo"), String::from("bar")])
        );

        assert_eq!(
            VectorMatchCardinality::OneToMany(HashSet::from([
                String::from("foo"),
                String::from("bar")
            ]))
            .labels()
            .unwrap(),
            &HashSet::from([String::from("foo"), String::from("bar")])
        );

        assert_eq!(
            VectorMatchCardinality::ManyToOne(HashSet::from([
                String::from("foo"),
                String::from("bar")
            ]))
            .labels()
            .unwrap(),
            &HashSet::from([String::from("foo"), String::from("bar")])
        );

        assert_eq!(VectorMatchCardinality::OneToOne.labels(), None);
        assert_eq!(VectorMatchCardinality::ManyToMany.labels(), None);
    }

    #[test]
    fn test_neg() {
        assert_eq!(
            -VectorSelector::from("foo"),
            UnaryExpr {
                expr: Box::new(Expr::from(VectorSelector::from("foo")))
            }
        )
    }

    #[test]
    fn test_scalar_value() {
        assert_eq!(Some(1.0), Expr::from(1.0).scalar_value());
        assert_eq!(None, Expr::from("1.0").scalar_value());
    }

    #[test]
    fn test_at_expr() {
        assert_eq!(
            "@ <timestamp> may not be set multiple times",
            Expr::from(VectorSelector::from("foo"))
                .at_expr(AtModifier::try_from(1.0).unwrap())
                .and_then(|ex| ex.at_expr(AtModifier::try_from(1.0).unwrap()))
                .unwrap_err()
        );

        assert_eq!(
            "@ <timestamp> may not be set multiple times",
            Expr::new_matrix_selector(
                Expr::from(VectorSelector::from("foo")),
                Duration::from_secs(1),
            )
            .and_then(|ex| ex.at_expr(AtModifier::try_from(1.0).unwrap()))
            .and_then(|ex| ex.at_expr(AtModifier::try_from(1.0).unwrap()))
            .unwrap_err()
        );

        assert_eq!(
            "@ <timestamp> may not be set multiple times",
            Expr::new_subquery_expr(
                Expr::from(VectorSelector::from("foo")),
                Duration::from_secs(1),
                None,
            )
            .and_then(|ex| ex.at_expr(AtModifier::try_from(1.0).unwrap()))
            .and_then(|ex| ex.at_expr(AtModifier::try_from(1.0).unwrap()))
            .unwrap_err()
        )
    }

    #[test]
    fn test_offset_expr() {
        assert_eq!(
            "offset may not be set multiple times",
            Expr::from(VectorSelector::from("foo"))
                .offset_expr(Offset::Pos(Duration::from_secs(1000)))
                .and_then(|ex| ex.offset_expr(Offset::Pos(Duration::from_secs(1000))))
                .unwrap_err()
        );

        assert_eq!(
            "offset may not be set multiple times",
            Expr::new_matrix_selector(
                Expr::from(VectorSelector::from("foo")),
                Duration::from_secs(1),
            )
            .and_then(|ex| ex.offset_expr(Offset::Pos(Duration::from_secs(1000))))
            .and_then(|ex| ex.offset_expr(Offset::Pos(Duration::from_secs(1000))))
            .unwrap_err()
        );

        assert_eq!(
            "offset may not be set multiple times",
            Expr::new_subquery_expr(
                Expr::from(VectorSelector::from("foo")),
                Duration::from_secs(1),
                None,
            )
            .and_then(|ex| ex.offset_expr(Offset::Pos(Duration::from_secs(1000))))
            .and_then(|ex| ex.offset_expr(Offset::Pos(Duration::from_secs(1000))))
            .unwrap_err()
        );
    }
}
