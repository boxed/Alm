//! Binary operator resolution: rebuild flat parser chains into trees
//! using precedence and associativity (port of the logic in
//! Canonicalize.Expression).

use super::*;

pub(super) fn resolve_binops(
    env: &Env,
    pairs: Vec<(can::Expr, Located<Name>)>,
    last: can::Expr,
) -> CResult<can::Expr> {
    let mut exprs = Vec::new();
    let mut ops: Vec<(Located<Name>, BinopEntry)> = Vec::new();
    for (expr, op) in pairs {
        let entry = env.binops.get(&op.value).ok_or_else(|| {
            Error::new(
                format!("I do not recognize the `{}` operator.", op.value),
                op.region,
            )
        })?;
        exprs.push(expr);
        ops.push((op, entry.clone()));
    }
    exprs.push(last);

    let mut pos = 0;
    let result = climb(&mut exprs.into_iter().map(Some).collect(), &ops, &mut pos, 0)?;
    debug_assert_eq!(pos, ops.len());
    Ok(result)
}

fn climb(
    exprs: &mut Vec<Option<can::Expr>>,
    ops: &[(Located<Name>, BinopEntry)],
    pos: &mut usize,
    min_precedence: u8,
) -> CResult<can::Expr> {
    let mut lhs = exprs[*pos].take().unwrap();
    while *pos < ops.len() && ops[*pos].1.precedence >= min_precedence {
        let (op, entry) = &ops[*pos];
        *pos += 1;

        // Everything binding tighter than this operator goes into the rhs.
        let next_min = match entry.associativity {
            Associativity::Left | Associativity::Non => entry.precedence + 1,
            Associativity::Right => entry.precedence,
        };
        // The rhs starts at the expression slot just after this operator.
        let rhs = climb(exprs, ops, pos, next_min)?;

        if entry.associativity == Associativity::Non
            && *pos < ops.len()
            && ops[*pos].1.precedence == entry.precedence
            && ops[*pos].1.associativity == Associativity::Non
        {
            return Err(Error::new(
                format!(
                    "You cannot chain the non-associative operators `{}` and `{}` without parentheses.",
                    op.value, ops[*pos].0.value
                ),
                ops[*pos].0.region,
            ));
        }

        let op_region = lhs.region.merge(rhs.region);
        lhs = Located::new(
            op_region,
            can::Expr_::Binop(
                op.value.clone(),
                entry.home.clone(),
                entry.function.clone(),
                Box::new(lhs),
                Box::new(rhs),
            ),
        );
    }
    Ok(lhs)
}
