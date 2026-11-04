//! Expression evaluation with SQL NULL semantics.

use std::collections::HashMap;
use xdb_ir::{errcode, BinOp, ColId, Expr, Row, UnOp, Value, VarId};

/// A variable slot: scalar or row record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot {
    S(Value),
    R(Option<Row>),
}

pub type EResult = Result<Value, i32>;

pub trait Env {
    fn var(&self, v: VarId) -> EResult;
    fn col(&self, v: VarId, c: ColId) -> EResult;
    fn old(&self, _c: ColId) -> EResult {
        Err(errcode::VALUE_ERROR)
    }
}

fn slot_var(s: Option<&Slot>) -> EResult {
    match s {
        Some(Slot::S(v)) => Ok(v.clone()),
        _ => Err(errcode::VALUE_ERROR),
    }
}

fn slot_col(s: Option<&Slot>, c: ColId) -> EResult {
    match s {
        Some(Slot::R(Some(row))) => row.get(c as usize).cloned().ok_or(errcode::VALUE_ERROR),
        Some(Slot::R(None)) => Ok(Value::Null),
        _ => Err(errcode::VALUE_ERROR),
    }
}

/// Variables of a session, optionally overlaid by values forwarded inside the
/// lookahead window.
pub struct VarsEnv<'a> {
    pub vars: &'a [Slot],
    pub fwd: Option<&'a HashMap<VarId, Option<Slot>>>,
}

impl<'a> VarsEnv<'a> {
    fn get(&self, v: VarId) -> Option<&Slot> {
        if let Some(f) = self.fwd {
            if let Some(Some(s)) = f.get(&v) {
                return Some(s);
            }
        }
        self.vars.get(v as usize)
    }
}

impl<'a> Env for VarsEnv<'a> {
    fn var(&self, v: VarId) -> EResult {
        slot_var(self.get(v))
    }
    fn col(&self, v: VarId, c: ColId) -> EResult {
        slot_col(self.get(v), c)
    }
}

/// Evaluation at a shard: `Col(_, c)` and `OldCol(c)` read the given row. All other
/// variables must have been substituted by constants.
pub struct RowEnv<'a> {
    pub row: &'a Row,
}

impl<'a> Env for RowEnv<'a> {
    fn var(&self, _v: VarId) -> EResult {
        Err(errcode::VALUE_ERROR)
    }
    fn col(&self, _v: VarId, c: ColId) -> EResult {
        self.row.get(c as usize).cloned().ok_or(errcode::VALUE_ERROR)
    }
    fn old(&self, c: ColId) -> EResult {
        self.row.get(c as usize).cloned().ok_or(errcode::VALUE_ERROR)
    }
}

fn tri(v: &Value) -> Result<Option<bool>, i32> {
    match v {
        Value::Null => Ok(None),
        Value::Int(i) => Ok(Some(*i != 0)),
        Value::Str(_) => Err(errcode::VALUE_ERROR),
    }
}

fn from_tri(b: Option<bool>) -> Value {
    match b {
        None => Value::Null,
        Some(b) => Value::Int(b as i64),
    }
}

fn to_int(v: &Value) -> Result<i64, i32> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Str(s) => s.trim().parse::<i64>().map_err(|_| errcode::VALUE_ERROR),
        Value::Null => Err(errcode::VALUE_ERROR),
    }
}

pub fn eval(e: &Expr, env: &dyn Env) -> EResult {
    match e {
        Expr::Const(v) => Ok(v.clone()),
        Expr::Var(v) => env.var(*v),
        Expr::Col(v, c) => env.col(*v, *c),
        Expr::OldCol(c) => env.old(*c),
        Expr::IsNull(a) => Ok(Value::Int(matches!(eval(a, env)?, Value::Null) as i64)),
        Expr::Coalesce(list) => {
            for x in list {
                let v = eval(x, env)?;
                if v != Value::Null {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        Expr::Unary(UnOp::Neg, a) => match eval(a, env)? {
            Value::Null => Ok(Value::Null),
            v => to_int(&v)?.checked_neg().map(Value::Int).ok_or(errcode::VALUE_ERROR),
        },
        Expr::Unary(UnOp::Not, a) => Ok(from_tri(tri(&eval(a, env)?)?.map(|b| !b))),
        Expr::Binary(BinOp::And, a, b) => {
            let x = tri(&eval(a, env)?)?;
            if x == Some(false) {
                return Ok(Value::Int(0));
            }
            let y = tri(&eval(b, env)?)?;
            Ok(match (x, y) {
                (_, Some(false)) => Value::Int(0),
                (Some(true), Some(true)) => Value::Int(1),
                _ => Value::Null,
            })
        }
        Expr::Binary(BinOp::Or, a, b) => {
            let x = tri(&eval(a, env)?)?;
            if x == Some(true) {
                return Ok(Value::Int(1));
            }
            let y = tri(&eval(b, env)?)?;
            Ok(match (x, y) {
                (_, Some(true)) => Value::Int(1),
                (Some(false), Some(false)) => Value::Int(0),
                _ => Value::Null,
            })
        }
        Expr::Binary(op, a, b) => {
            let x = eval(a, env)?;
            let y = eval(b, env)?;
            binop(*op, x, y)
        }
    }
}

fn binop(op: BinOp, x: Value, y: Value) -> EResult {
    use Value::*;
    if op == BinOp::Concat {
        if x == Null && y == Null {
            return Ok(Null);
        }
        let s = |v: &Value| match v {
            Null => String::new(),
            Int(i) => i.to_string(),
            Str(s) => s.clone(),
        };
        return Ok(Str(s(&x) + &s(&y)));
    }
    if x == Null || y == Null {
        return Ok(Null);
    }
    let cmp = |o: std::cmp::Ordering| -> EResult {
        use std::cmp::Ordering::*;
        let b = match op {
            BinOp::Eq => o == Equal,
            BinOp::Ne => o != Equal,
            BinOp::Lt => o == Less,
            BinOp::Le => o != Greater,
            BinOp::Gt => o == Greater,
            BinOp::Ge => o != Less,
            _ => unreachable!(),
        };
        Ok(Int(b as i64))
    };
    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match (&x, &y) {
            (Str(a), Str(b)) => cmp(a.cmp(b)),
            _ => cmp(to_int(&x)?.cmp(&to_int(&y)?)),
        },
        _ => {
            let a = to_int(&x)?;
            let b = to_int(&y)?;
            let r = match op {
                BinOp::Add => a.checked_add(b),
                BinOp::Sub => a.checked_sub(b),
                BinOp::Mul => a.checked_mul(b),
                BinOp::Div => {
                    if b == 0 {
                        return Err(errcode::ZERO_DIVIDE);
                    }
                    a.checked_div(b)
                }
                BinOp::Mod => {
                    if b == 0 {
                        Some(a) // Oracle: MOD(m, 0) = m
                    } else {
                        a.checked_rem(b)
                    }
                }
                _ => unreachable!(),
            };
            r.map(Int).ok_or(errcode::VALUE_ERROR)
        }
    }
}

/// Replace variables by their current values. `Col(keep, _)` and `OldCol` are kept,
/// so the result can be evaluated at a shard against a row.
pub fn subst(e: &Expr, env: &dyn Env, keep: Option<VarId>) -> Result<Expr, i32> {
    Ok(match e {
        Expr::Const(_) | Expr::OldCol(_) => e.clone(),
        Expr::Var(v) => Expr::Const(env.var(*v)?),
        Expr::Col(v, c) => {
            if Some(*v) == keep {
                e.clone()
            } else {
                Expr::Const(env.col(*v, *c)?)
            }
        }
        Expr::Unary(o, a) => Expr::Unary(*o, Box::new(subst(a, env, keep)?)),
        Expr::Binary(o, a, b) => {
            Expr::Binary(*o, Box::new(subst(a, env, keep)?), Box::new(subst(b, env, keep)?))
        }
        Expr::IsNull(a) => Expr::IsNull(Box::new(subst(a, env, keep)?)),
        Expr::Coalesce(l) => {
            Expr::Coalesce(l.iter().map(|x| subst(x, env, keep)).collect::<Result<_, _>>()?)
        }
    })
}

/// Variables read by an expression.
pub fn expr_vars(e: &Expr, out: &mut Vec<VarId>) {
    match e {
        Expr::Const(_) | Expr::OldCol(_) => {}
        Expr::Var(v) | Expr::Col(v, _) => out.push(*v),
        Expr::Unary(_, a) | Expr::IsNull(a) => expr_vars(a, out),
        Expr::Binary(_, a, b) => {
            expr_vars(a, out);
            expr_vars(b, out);
        }
        Expr::Coalesce(l) => l.iter().for_each(|x| expr_vars(x, out)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct E;
    impl Env for E {
        fn var(&self, _: VarId) -> EResult {
            Ok(Value::Null)
        }
        fn col(&self, _: VarId, _: ColId) -> EResult {
            Ok(Value::Null)
        }
    }
    fn c(i: i64) -> Box<Expr> {
        Box::new(Expr::Const(Value::Int(i)))
    }
    #[test]
    fn arithmetic_and_nulls() {
        assert_eq!(eval(&Expr::Binary(BinOp::Add, c(2), c(3)), &E), Ok(Value::Int(5)));
        assert_eq!(eval(&Expr::Binary(BinOp::Div, c(7), c(0)), &E), Err(errcode::ZERO_DIVIDE));
        assert_eq!(eval(&Expr::Binary(BinOp::Mod, c(7), c(0)), &E), Ok(Value::Int(7)));
        let null = Box::new(Expr::Const(Value::Null));
        assert_eq!(eval(&Expr::Binary(BinOp::Add, c(1), null.clone()), &E), Ok(Value::Null));
        assert_eq!(eval(&Expr::Binary(BinOp::And, c(0), null.clone()), &E), Ok(Value::Int(0)));
        assert_eq!(eval(&Expr::Binary(BinOp::Or, c(1), null), &E), Ok(Value::Int(1)));
    }
}
