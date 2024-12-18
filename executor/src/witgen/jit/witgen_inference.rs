#![allow(unused)]
use std::collections::{HashMap, HashSet};

use itertools::Itertools;
use powdr_ast::analyzed::{
    AlgebraicBinaryOperation, AlgebraicBinaryOperator, AlgebraicExpression as Expression,
    AlgebraicReference, AlgebraicUnaryOperation, AlgebraicUnaryOperator, Identity, LookupIdentity,
    PermutationIdentity, PhantomLookupIdentity, PhantomPermutationIdentity, PolyID,
    PolynomialIdentity, PolynomialType, SelectedExpressions,
};
use powdr_number::FieldElement;

use crate::witgen::{
    global_constraints::RangeConstraintSet, jit::affine_symbolic_expression::MachineCallArgument,
};

use super::{
    super::{range_constraints::RangeConstraint, FixedData},
    affine_symbolic_expression::{AffineSymbolicExpression, Effect, ProcessResult},
    cell::Cell,
};

/// This component can generate code that solves identities.
/// It needs a driver that tells it which identities to process on which rows.
pub struct WitgenInference<'a, T: FieldElement, FixedEval: FixedEvaluator<T>> {
    fixed_data: &'a FixedData<'a, T>,
    fixed_evaluator: FixedEval,
    derived_range_constraints: HashMap<Cell, RangeConstraint<T>>,
    known_cells: HashSet<Cell>,
    code: Vec<Effect<T, Cell>>,
}

impl<'a, T: FieldElement, FixedEval: FixedEvaluator<T>> WitgenInference<'a, T, FixedEval> {
    pub fn new(
        fixed_data: &'a FixedData<'a, T>,
        fixed_evaluator: FixedEval,
        known_cells: impl IntoIterator<Item = Cell>,
    ) -> Self {
        Self {
            fixed_data,
            fixed_evaluator,
            derived_range_constraints: Default::default(),
            known_cells: known_cells.into_iter().collect(),
            code: Default::default(),
        }
    }

    pub fn code(self) -> Vec<Effect<T, Cell>> {
        self.code
    }

    /// Process an identity on a certain row.
    /// Returns true if this identity/row pair was fully processed and
    /// should not be considered again.
    pub fn process_identity(&mut self, id: &Identity<T>, row_offset: i32) -> bool {
        let result = match id {
            Identity::Polynomial(PolynomialIdentity { expression, .. }) => {
                self.process_polynomial_identity(expression, row_offset)
            }
            Identity::Lookup(LookupIdentity {
                id, left, right, ..
            })
            | Identity::Permutation(PermutationIdentity {
                id, left, right, ..
            })
            | Identity::PhantomPermutation(PhantomPermutationIdentity {
                id, left, right, ..
            })
            | Identity::PhantomLookup(PhantomLookupIdentity {
                id, left, right, ..
            }) => self.process_lookup(*id, left, right, row_offset),
            Identity::PhantomBusInteraction(_) => {
                // TODO(bus_interaction) Once we have a concept of "can_be_answered", bus interactions
                // should be as easy as lookups.
                ProcessResult::empty()
            }
            Identity::Connect(_) => ProcessResult::empty(),
        };
        self.ingest_effects(result.effects);
        result.complete
    }

    fn process_polynomial_identity(
        &self,
        expression: &'a Expression<T>,
        offset: i32,
    ) -> ProcessResult<T, Cell> {
        if let Some(r) = self.evaluate(expression, offset) {
            // TODO propagate or report error properly.
            // If solve returns an error, it means that the constraint is conflicting.
            // In the future, we might run this in a runtime-conditional, so an error
            // could just mean that this case cannot happen in practice.
            r.solve().unwrap()
        } else {
            ProcessResult::empty()
        }
    }

    fn process_lookup(
        &self,
        lookup_id: u64,
        left: &SelectedExpressions<T>,
        right: &SelectedExpressions<T>,
        offset: i32,
    ) -> ProcessResult<T, Cell> {
        // TODO: In the future, call the 'mutable state' to check if the
        // lookup can always be answered.

        // If the RHS is fully fixed columns...
        if right.expressions.iter().all(|e| match e {
            Expression::Reference(r) => r.is_fixed(),
            Expression::Number(_) => true,
            _ => false,
        }) {
            // and the selector is known to be 1...
            if self
                .evaluate(&left.selector, offset)
                .and_then(|s| s.try_to_known().map(|k| k.is_known_one()))
                == Some(true)
            {
                if let Some(lhs) = left
                    .expressions
                    .iter()
                    .map(|e| self.evaluate(e, offset))
                    .collect::<Option<Vec<_>>>()
                {
                    // and all except one expression is known on the LHS.
                    let unknown = lhs
                        .iter()
                        .filter(|e| e.try_to_known().is_none())
                        .collect_vec();
                    if unknown.len() == 1 && unknown[0].single_unknown_variable().is_some() {
                        let effects = vec![Effect::MachineCall(
                            lookup_id,
                            lhs.into_iter()
                                .map(|e| {
                                    if let Some(val) = e.try_to_known() {
                                        MachineCallArgument::Known(val.clone())
                                    } else {
                                        MachineCallArgument::Unknown(e)
                                    }
                                })
                                .collect(),
                        )];
                        return ProcessResult::complete(effects);
                    }
                }
            }
        }
        ProcessResult::empty()
    }

    fn ingest_effects(&mut self, effects: Vec<Effect<T, Cell>>) {
        for e in effects {
            match &e {
                Effect::Assignment(cell, assignment) => {
                    self.known_cells.insert(cell.clone());
                    if let Some(rc) = assignment.range_constraint() {
                        // If the cell was determined to be a constant, we add this
                        // as a range constraint, so we can use it in future evaluations.
                        self.add_range_constraint(cell.clone(), rc);
                    }
                    self.code.push(e);
                }
                Effect::RangeConstraint(cell, rc) => {
                    self.add_range_constraint(cell.clone(), rc.clone());
                }
                Effect::MachineCall(_, arguments) => {
                    for arg in arguments {
                        if let MachineCallArgument::Unknown(expr) = arg {
                            let cell = expr.single_unknown_variable().unwrap();
                            self.known_cells.insert(cell.clone());
                        }
                    }
                    self.code.push(e);
                }
                Effect::Assertion(_) => self.code.push(e),
            }
        }
    }

    fn add_range_constraint(&mut self, cell: Cell, rc: RangeConstraint<T>) {
        let rc = self
            .range_constraint(cell.clone())
            .map_or(rc.clone(), |existing_rc| existing_rc.conjunction(&rc));
        if !self.known_cells.contains(&cell) {
            if let Some(v) = rc.try_to_single_value() {
                // Special case: Cell is fixed to a constant by range constraints only.
                self.known_cells.insert(cell.clone());
                self.code.push(Effect::Assignment(cell.clone(), v.into()));
            }
        }
        self.derived_range_constraints.insert(cell.clone(), rc);
    }

    fn evaluate(
        &self,
        expr: &Expression<T>,
        offset: i32,
    ) -> Option<AffineSymbolicExpression<T, Cell>> {
        Some(match expr {
            Expression::Reference(r) => {
                if r.is_fixed() {
                    self.fixed_evaluator.evaluate(r, offset)?.into()
                } else {
                    let cell = Cell::from_reference(r, offset);
                    // If a cell is known and has a compile-time constant value,
                    // that value is stored in the range constraints.
                    let rc = self.range_constraint(cell.clone());
                    if let Some(val) = rc.as_ref().and_then(|rc| rc.try_to_single_value()) {
                        val.into()
                    } else if self.known_cells.contains(&cell) {
                        AffineSymbolicExpression::from_known_symbol(cell, rc)
                    } else {
                        AffineSymbolicExpression::from_unknown_variable(cell, rc)
                    }
                }
            }
            Expression::PublicReference(_) | Expression::Challenge(_) => {
                // TODO we need to introduce a variable type for those.
                return None;
            }
            Expression::Number(n) => (*n).into(),
            Expression::BinaryOperation(op) => self.evaluate_binary_operation(op, offset)?,
            Expression::UnaryOperation(op) => self.evaluate_unary_operation(op, offset)?,
        })
    }

    fn evaluate_binary_operation(
        &self,
        op: &AlgebraicBinaryOperation<T>,
        offset: i32,
    ) -> Option<AffineSymbolicExpression<T, Cell>> {
        let left = self.evaluate(&op.left, offset)?;
        let right = self.evaluate(&op.right, offset)?;
        match op.op {
            AlgebraicBinaryOperator::Add => Some(&left + &right),
            AlgebraicBinaryOperator::Sub => Some(&left - &right),
            AlgebraicBinaryOperator::Mul => left.try_mul(&right),
            AlgebraicBinaryOperator::Pow => {
                let result = left
                    .try_to_known()?
                    .try_to_number()?
                    .pow(right.try_to_known()?.try_to_number()?.to_integer());
                Some(AffineSymbolicExpression::from(result))
            }
        }
    }

    fn evaluate_unary_operation(
        &self,
        op: &AlgebraicUnaryOperation<T>,
        offset: i32,
    ) -> Option<AffineSymbolicExpression<T, Cell>> {
        let expr = self.evaluate(&op.expr, offset)?;
        match op.op {
            AlgebraicUnaryOperator::Minus => Some(-&expr),
        }
    }

    /// Returns the current best-known range constraint on the given cell
    /// combining global range constraints and newly derived local range constraints.
    fn range_constraint(&self, cell: Cell) -> Option<RangeConstraint<T>> {
        self.fixed_data
            .global_range_constraints
            .range_constraint(&AlgebraicReference {
                name: Default::default(),
                poly_id: PolyID {
                    id: cell.id,
                    ptype: PolynomialType::Committed,
                },
                next: false,
            })
            .iter()
            .chain(self.derived_range_constraints.get(&cell))
            .cloned()
            .reduce(|gc, rc| gc.conjunction(&rc))
    }
}

pub trait FixedEvaluator<T: FieldElement> {
    fn evaluate(&self, _var: &AlgebraicReference, _row_offset: i32) -> Option<T> {
        None
    }
}

#[cfg(test)]
mod test {

    use pretty_assertions::assert_eq;

    use powdr_ast::analyzed::Analyzed;
    use powdr_number::GoldilocksField;

    use crate::{
        constant_evaluator,
        witgen::{global_constraints, jit::affine_symbolic_expression::Assertion, FixedData},
    };

    use super::*;

    fn format_code(effects: &[Effect<GoldilocksField, Cell>]) -> String {
        effects
            .iter()
            .map(|effect| match effect {
                Effect::Assignment(v, expr) => format!("{v} = {expr};"),
                Effect::Assertion(Assertion {
                    lhs,
                    rhs,
                    expected_equal,
                }) => {
                    format!(
                        "assert {lhs} {} {rhs};",
                        if *expected_equal { "==" } else { "!=" }
                    )
                }
                Effect::MachineCall(id, args) => {
                    format!(
                        "lookup({id}, [{}]);",
                        args.iter()
                            .map(|arg| match arg {
                                MachineCallArgument::Known(k) => format!("Known({k})"),
                                MachineCallArgument::Unknown(u) => format!("Unknown({u})"),
                            })
                            .join(", ")
                    )
                }
                Effect::RangeConstraint(..) => {
                    panic!("Range constraints should not be part of the code.")
                }
            })
            .join("\n")
    }

    struct FixedEvaluatorForFixedData<'a>(&'a FixedData<'a, GoldilocksField>);
    impl<'a> FixedEvaluator<GoldilocksField> for FixedEvaluatorForFixedData<'a> {
        fn evaluate(&self, var: &AlgebraicReference, row_offset: i32) -> Option<GoldilocksField> {
            assert!(var.is_fixed());
            let values = self.0.fixed_cols[&var.poly_id].values_max_size();
            let row = (row_offset as usize + var.next as usize) % values.len();
            Some(values[row])
        }
    }

    fn solve_on_rows(
        input: &str,
        rows: &[i32],
        known_cells: Vec<(&str, i32)>,
        expected_complete: Option<usize>,
    ) -> String {
        let analyzed: Analyzed<GoldilocksField> =
            powdr_pil_analyzer::analyze_string(input).unwrap();
        let fixed_col_vals = constant_evaluator::generate(&analyzed);
        let fixed_data = FixedData::new(&analyzed, &fixed_col_vals, &[], Default::default(), 0);
        let (fixed_data, retained_identities) =
            global_constraints::set_global_constraints(fixed_data, &analyzed.identities);
        let known_cells = known_cells.iter().map(|(name, row_offset)| {
            let id = fixed_data.try_column_by_name(name).unwrap().id;
            Cell {
                column_name: name.to_string(),
                id,
                row_offset: *row_offset,
            }
        });

        let ref_eval = FixedEvaluatorForFixedData(&fixed_data);
        let mut witgen = WitgenInference::new(&fixed_data, ref_eval, known_cells);
        let mut complete = HashSet::new();
        let mut counter = 0;
        let expected_complete = expected_complete.unwrap_or(retained_identities.len() * rows.len());
        while complete.len() != expected_complete {
            counter += 1;
            for row in rows {
                for id in retained_identities.iter() {
                    if !complete.contains(&(id.id(), *row)) && witgen.process_identity(id, *row) {
                        complete.insert((id.id(), *row));
                    }
                }
            }
            assert!(counter < 10000, "Solving took more than 10000 rounds.");
        }
        format_code(&witgen.code())
    }

    #[test]
    fn simple_polynomial_solving() {
        let input = "let X; let Y; let Z; X = 1; Y = X + 1; Z * Y = X + 10;";
        let code = solve_on_rows(input, &[0], vec![], None);
        assert_eq!(code, "X[0] = 1;\nY[0] = 2;\nZ[0] = -9223372034707292155;");
    }

    #[test]
    fn fib() {
        let input = "let X; let Y; X' = Y; Y' = X + Y;";
        let code = solve_on_rows(input, &[0, 1], vec![("X", 0), ("Y", 0)], None);
        assert_eq!(
            code,
            "X[1] = Y[0];\nY[1] = (X[0] + Y[0]);\nX[2] = Y[1];\nY[2] = (X[1] + Y[1]);"
        );
    }

    #[test]
    fn fib_with_fixed() {
        let input = "
        namespace Fib(8);
            col fixed FIRST = [1] + [0]*;
            let x;
            let y;
            FIRST * (y - 1) = 0;
            FIRST * (x - 1) = 0;
            // This works in this test because we do not implement wrapping properly in this test.
            x' - y = 0;
            y' - (x + y) = 0;
        ";
        let code = solve_on_rows(input, &[0, 1, 2, 3], vec![], None);
        assert_eq!(
            code,
            "Fib::y[0] = 1;
Fib::x[0] = 1;
Fib::x[1] = 1;
Fib::y[1] = 2;
Fib::x[2] = 2;
Fib::y[2] = 3;
Fib::x[3] = 3;
Fib::y[3] = 5;
Fib::x[4] = 5;
Fib::y[4] = 8;"
        );
    }

    #[test]
    fn xor() {
        let input = "
namespace Xor(256 * 256);
    let latch: col = |i| { if (i % 4) == 3 { 1 } else { 0 } };
    let FACTOR: col = |i| { 1 << (((i + 1) % 4) * 8) };

    let a: int -> int = |i| i % 256;
    let b: int -> int = |i| (i / 256) % 256;
    let P_A: col = a;
    let P_B: col = b;
    let P_C: col = |i| a(i) ^ b(i);

    let A_byte;
    let B_byte;
    let C_byte;

    [ A_byte, B_byte, C_byte ] in [ P_A, P_B, P_C ];

    let A;
    let B;
    let C;

    A' = A * (1 - latch) + A_byte * FACTOR;
    B' = B * (1 - latch) + B_byte * FACTOR;
    C' = C * (1 - latch) + C_byte * FACTOR;
";
        let code = solve_on_rows(
            input,
            // Use the second block to avoid wrap-around.
            &[3, 4, 5, 6, 7],
            vec![
                ("Xor::A", 7),
                ("Xor::C", 7), // We solve it in reverse, just for fun.
            ],
            Some(16),
        );
        assert_eq!(
            code,
            "\
Xor::A_byte[6] = ((Xor::A[7] & 4278190080) // 16777216);
Xor::A[6] = (Xor::A[7] & 16777215);
assert Xor::A[7] == (Xor::A[7] | 4294967295);
Xor::C_byte[6] = ((Xor::C[7] & 4278190080) // 16777216);
Xor::C[6] = (Xor::C[7] & 16777215);
assert Xor::C[7] == (Xor::C[7] | 4294967295);
Xor::A_byte[5] = ((Xor::A[6] & 16711680) // 65536);
Xor::A[5] = (Xor::A[6] & 65535);
assert Xor::A[6] == (Xor::A[6] | 16777215);
Xor::C_byte[5] = ((Xor::C[6] & 16711680) // 65536);
Xor::C[5] = (Xor::C[6] & 65535);
assert Xor::C[6] == (Xor::C[6] | 16777215);
lookup(0, [Known(Xor::A_byte[6]), Unknown(Xor::B_byte[6]), Known(Xor::C_byte[6])]);
Xor::A_byte[4] = ((Xor::A[5] & 65280) // 256);
Xor::A[4] = (Xor::A[5] & 255);
assert Xor::A[5] == (Xor::A[5] | 65535);
Xor::C_byte[4] = ((Xor::C[5] & 65280) // 256);
Xor::C[4] = (Xor::C[5] & 255);
assert Xor::C[5] == (Xor::C[5] | 65535);
lookup(0, [Known(Xor::A_byte[5]), Unknown(Xor::B_byte[5]), Known(Xor::C_byte[5])]);
Xor::A_byte[3] = Xor::A[4];
Xor::C_byte[3] = Xor::C[4];
lookup(0, [Known(Xor::A_byte[4]), Unknown(Xor::B_byte[4]), Known(Xor::C_byte[4])]);
lookup(0, [Known(Xor::A_byte[3]), Unknown(Xor::B_byte[3]), Known(Xor::C_byte[3])]);
Xor::B[4] = Xor::B_byte[3];
Xor::B[5] = (Xor::B[4] + (Xor::B_byte[4] * 256));
Xor::B[6] = (Xor::B[5] + (Xor::B_byte[5] * 65536));
Xor::B[7] = (Xor::B[6] + (Xor::B_byte[6] * 16777216));"
        );
    }
}
