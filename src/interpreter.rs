
use crate::{model::{Value, Env, RuntimeError, Lambda, List}};
use std::{collections::BTreeMap, rc::Rc, cell::{RefCell}};

use solana_program::account_info::AccountInfo;

/// Evaluate a single Lisp expression in the context of a given environment.
pub fn eval(env: Rc<RefCell<Env>>, expression: &Value, ctx: &Vec<AccountInfo>) -> Result<Value,RuntimeError> {
  eval_inner(env, ctx, expression, false, false)
}

/// Evaluate a series of s-expressions. Each expression is evaluated in 
/// order and the final one's return value is returned.
pub fn eval_block(env: Rc<RefCell<Env>>, ctx: &Vec<AccountInfo>, clauses: impl Iterator<Item=Value>) -> Result<Value,RuntimeError> {
  eval_block_inner(env, ctx, clauses, false, false)
}

fn eval_block_inner(env: Rc<RefCell<Env>>, ctx: &Vec<AccountInfo>, clauses: impl Iterator<Item=Value>, found_tail: bool, in_func: bool) -> Result<Value,RuntimeError> {

  let mut current_expr: Option<Value> = None;
  for clause in clauses {
    if let Some(expr) = current_expr {
      match eval_inner(env.clone(), ctx, &expr, true, in_func) {
        Ok(_) => (),
        Err(e) => {
          return Err(e);
        }
      }
    }

    current_expr = Some(clause);
  }

  return eval_inner(env.clone(), ctx, &current_expr.unwrap(), found_tail, in_func);
}

/// `found_tail` and `in_func` are used when locating the tail position for 
/// tail-call optimization. Candidates are not eligible if a) we aren't already 
/// inside a function call, or b) we've already found the tail inside the current
/// function call. `found_tail` is currently overloaded inside special forms to 
/// factor out function calls in, say, the conditional slot, which are not 
/// eligible to be the tail-call based on their position. A future refactor hopes
/// to make things a little more semantic.
fn eval_inner(env: Rc<RefCell<Env>>, ctx: &Vec<AccountInfo>, expression: &Value, found_tail: bool, in_func: bool) -> Result<Value,RuntimeError> {

  let result: Result<Value,RuntimeError> = match expression {

    // look up symbol
    Value::Symbol(symbol) => match env.borrow().find(&symbol) {
      Some(expr) => Ok(expr.clone()),
      None => Err(RuntimeError { msg: format!("\"{}\" is not defined", symbol) }),
    },

    // s-expression
    Value::List(list) => {
      match &list.car()? {

        // special forms
        Value::Symbol(symbol) if symbol == "define" => {
          let cdr = list.cdr();
          let symbol = cdr.car()?.as_symbol().unwrap();
          let value_expr = &cdr.cdr().car()?;
          let value = eval_inner(env.clone(), ctx, value_expr, true, in_func)?;

          env.borrow_mut().entries.insert(symbol, value.clone());

          Ok(value)
        },

        Value::Symbol(symbol) if symbol == "set" => {
          let cdr = list.cdr();
          let symbol = cdr.car()?.as_symbol().unwrap();
          let value_expr = &cdr.cdr().car()?;
          let value = eval_inner(env.clone(), ctx, value_expr, true, in_func)?;

          if env.borrow().entries.contains_key(&symbol) {
            env.borrow_mut().entries.insert(symbol, value.clone());
          } else {
            let mut focal_env: Option<Rc<RefCell<Env>>> = env.borrow().parent.clone();
  
            while focal_env.as_ref().map_or(false, |e| !e.borrow().entries.contains_key(&symbol)) {
              let rc = focal_env.unwrap();
              focal_env = rc.borrow().parent.clone();
            }

            if let Some(env) = focal_env {
              env.borrow_mut().entries.insert(symbol, value.clone());
            } else {
              return Err(RuntimeError { msg: format!("Tried to set value of undefined symbol \"{}\"", symbol) });
            }
          }

          Ok(value)
        },

        Value::Symbol(symbol) if symbol == "defun" => {
          let mut list_iter = list.into_iter();
          list_iter.next().unwrap(); // skip "defun"
          let symbol = list_iter.next().unwrap().as_symbol().unwrap();
          let argnames = Rc::new(list_iter.next().unwrap().clone());
          let body = Rc::new(Value::List(list_iter.map(|v| v.clone()).collect::<List>()));

          let lambda = Value::Lambda(Lambda {
            closure: env.clone(),
            argnames,
            body
          });

          env.borrow_mut().entries.insert(symbol, lambda);

          Ok(Value::NIL)
        },

        Value::Symbol(symbol) if symbol == "lambda" => {
          let cdr = list.cdr();
          let argnames = Rc::new(cdr.car()?.clone());
          let body = Rc::new(Value::List(cdr.cdr()));

          Ok(Value::Lambda(Lambda {
            closure: env.clone(),
            argnames,
            body
          }))
        },

        Value::Symbol(symbol) if symbol == "quote" => {
          let exp = list.cdr().car()?.clone();

          Ok(exp)
        },

        Value::Symbol(symbol) if symbol == "let" => {
          let let_env = Rc::new(RefCell::new(Env {
            parent: Some(env.clone()),
            entries: BTreeMap::new()
          }));
          let declarations = list.cdr().car()?;

          for decl in declarations.as_list().unwrap().into_iter() {
            let decl_cons = decl.as_list().unwrap();
            let symbol = decl_cons.car()?.as_symbol().unwrap();
            let expr = &decl_cons.cdr().car()?;

            let result = eval_inner(let_env.clone(), ctx, &expr, true, in_func)?;
            let_env.borrow_mut().entries.insert(symbol, result);
          }

          let body = Value::List(list.cdr().cdr());

          eval_block_inner(let_env.clone(), ctx, body.as_list().unwrap().into_iter(), found_tail, in_func)
        },

        Value::Symbol(symbol) if symbol == "begin" => {
          let body = Value::List(list.cdr());

          eval_block_inner(env.clone(), ctx, body.as_list().unwrap().into_iter(), found_tail, in_func)
        },

        Value::Symbol(symbol) if symbol == "cond" => {
          let clauses = list.cdr();
          let mut result = Value::NIL;

          for clause in clauses.into_iter().map(|clause| clause.as_list().unwrap()) {
            let condition = &clause.car()?;
            let then = &clause.cdr().car()?;

            if eval_inner(env.clone(), ctx, condition, true, in_func)?.is_truthy() {
              result = eval_inner(env.clone(), ctx, then, found_tail, in_func)?;
              break;
            }
          }

          Ok(result)
        },

        Value::Symbol(symbol) if symbol == "if" => {
          let cdr = list.cdr();
          let condition = &cdr.car()?;
          let then_result = &cdr.cdr().car()?;
          let else_result = cdr.cdr().cdr().car().ok();

          if eval_inner(env.clone(), ctx, condition, true, in_func)?.is_truthy() {
            Ok(eval_inner(env.clone(), ctx, then_result, found_tail, in_func)?)
          } else {
            Ok(match else_result {
              Some(v) => eval_inner(env.clone(), ctx, &v, found_tail, in_func)?,
              None => Value::NIL
            })
          }
        },

        Value::Symbol(symbol) if symbol == "and" => {
          let cdr = list.cdr();
          let a = &cdr.car()?;
          let b = &cdr.cdr().car()?;

          Ok(Value::from_truth(
              eval_inner(env.clone(), ctx, a, true, in_func)?.is_truthy() 
              && eval_inner(env.clone(), ctx, b, true, in_func)?.is_truthy()
          ))
        },

        Value::Symbol(symbol) if symbol == "or" => {
          let cdr = list.cdr();
          let a = &cdr.car()?;
          let b = &cdr.cdr().car()?;

          Ok(Value::from_truth(
              eval_inner(env.clone(), ctx, a, true, in_func)?.is_truthy() 
              || eval_inner(env.clone(), ctx, b, true, in_func)?.is_truthy()
          ))
        },


        // function call
        _ => {
          let func = eval_inner(env.clone(), ctx, &list.car()?, true, in_func)?;
          let args = list.into_iter().skip(1)
            .map(|car| eval_inner(env.clone(), ctx, &car, true, in_func).map_err(|e| e.clone()));

          if !found_tail && in_func {
            let args_vec = args
              .filter_map(|a| a.clone().ok())
              .collect();

            let expr = Value::TailCall {
              func: Rc::new(func),
              args: args_vec
            };

            return Ok(expr);
          } else {

            let mut res = call_function(env.clone(), &func, args.collect(), ctx);
            while let Ok(Value::TailCall { func, args }) = res {
              res = call_function(env.clone(), &func, args.iter().map(|arg| Ok(arg.clone())).collect(), ctx);
            }

            res
          }
        }
      }
    },

    // plain value
    _ => Ok(expression.clone()),
  };

  return result;
}

/// Calling a function is separated from the main `eval_inner()` function
/// so that tail calls can be evaluated without just returning themselves 
/// as-is as a tail-call.
fn call_function(env: Rc<RefCell<Env>>, func: &Value, args: Vec<Result<Value,RuntimeError>>, ctx: &Vec<AccountInfo>) -> Result<Value, RuntimeError> {
  match func {

    // call native function
    Value::NativeFunc(func) => {
      let err = args.iter()
        .find_map(|a| a.clone().err());
      
      let args_vec = args.iter()
        .filter_map(|a| a.clone().ok())
        .collect();

      match err {
        Some(e) => Err(e),
        None => func(env.clone(), &args_vec, ctx)
      }
    },

    // call lambda function
    Value::Lambda(lamb) => {
      let argnames = lamb.argnames.as_list().unwrap();

      // bind args
      let mut entries: BTreeMap<String,Value> = BTreeMap::new();
      
      for (index, arg_name) in argnames.into_iter().enumerate() {
        let name = arg_name.as_symbol().unwrap();

        if name == "..." {  // rest parameters
          entries.insert(String::from("..."), Value::List(args.into_iter().skip(index).filter_map(|a| a.clone().ok()).collect::<List>()));
          break;
        } else {
          entries.insert(name, args[index].clone()?);
        }
      }

      let arg_env = Rc::new(RefCell::new(Env {
        parent: Some(env.clone()),
        entries
      }));
          
      // evaluate each line of body
      eval_block_inner(arg_env.clone(), ctx, lamb.body.as_list().unwrap().into_iter(), false, true)
    }
    _ => Err(RuntimeError { msg: String::from("Argument 0 is not callable") })
  }
}
