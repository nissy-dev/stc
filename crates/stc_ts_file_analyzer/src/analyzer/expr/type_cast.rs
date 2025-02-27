use std::borrow::Cow;

use stc_ts_ast_rnode::{RTsAsExpr, RTsLit, RTsTypeAssertion};
use stc_ts_errors::{DebugExt, Error};
use stc_ts_types::{Interface, KeywordType, LitType, TypeElement, TypeParamInstantiation};
use stc_utils::cache::Freeze;
use swc_common::{Span, Spanned, TypeEq};
use swc_ecma_ast::TsKeywordTypeKind;

use crate::{
    analyzer::{
        assign::AssignOpts,
        expr::TypeOfMode,
        scope::ExpandOpts,
        util::{make_instance_type, ResultExt},
        Analyzer,
    },
    ty::Type,
    util::is_str_or_union,
    validator,
    validator::ValidateWith,
    VResult,
};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CastableOpts {
    /// `true` if we are checking for `A extends B` relation.
    pub disallow_different_classes: bool,

    pub allow_assignment_to_param_constraint: bool,

    pub disallow_special_assignment_to_empty_class: bool,
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(
        &mut self,
        e: &RTsTypeAssertion,
        mode: TypeOfMode,
        type_args: Option<&TypeParamInstantiation>,
        type_ann: Option<&Type>,
    ) -> VResult {
        // We don't apply type annotation because it can corrupt type checking.
        let mut casted_ty = e.type_ann.validate_with(self)?;
        casted_ty.make_clone_cheap();
        let mut orig_ty = e.expr.validate_with_args(self, (mode, type_args, Some(&casted_ty)))?;
        orig_ty.make_clone_cheap();

        self.validate_type_cast(e.span, orig_ty, casted_ty)
    }
}

#[validator]
impl Analyzer<'_, '_> {
    fn validate(
        &mut self,
        e: &RTsAsExpr,
        mode: TypeOfMode,
        type_args: Option<&TypeParamInstantiation>,
        type_ann: Option<&Type>,
    ) -> VResult {
        if e.node_id.is_invalid() {
            return e.type_ann.validate_with(self);
        }

        // We don't apply type annotation because it can corrupt type checking.
        let casted_ty = e.type_ann.validate_with(self)?;
        let orig_ty = e.expr.validate_with_args(self, (mode, type_args, Some(&casted_ty)))?;

        self.validate_type_cast(e.span, orig_ty, casted_ty)
    }
}

impl Analyzer<'_, '_> {
    /// ```ts
    /// var unionTuple3: [number, string | number] = [10, "foo"];
    /// var unionTuple4 = <[number, number]>unionTuple3;
    /// ```
    ///
    /// is valid, while
    ///
    /// ```ts
    /// var unionTuple3: [number, string | number] = [10, "foo"];
    /// var unionTuple4: [number, number] = unionTuple3;
    /// ```
    ///
    /// results in error.
    fn validate_type_cast(&mut self, span: Span, orig_ty: Type, casted_ty: Type) -> VResult {
        let mut orig_ty = self.expand(
            span,
            orig_ty,
            ExpandOpts {
                full: true,
                expand_union: true,
                ..Default::default()
            },
        )?;
        orig_ty.make_clone_cheap();

        let mut casted_ty = make_instance_type(self.ctx.module_id, casted_ty);
        self.prevent_inference_while_simplifying(&mut casted_ty);
        casted_ty = self.simplify(casted_ty);

        self.prevent_expansion(&mut casted_ty);
        casted_ty.make_clone_cheap();

        self.validate_type_cast_inner(span, &orig_ty, &casted_ty).report(&mut self.storage);

        Ok(casted_ty)
    }

    fn validate_type_cast_inner(&mut self, span: Span, orig: &Type, casted: &Type) -> VResult<()> {
        // I don't know why this is valid, but `stringLiteralsWithTypeAssertions01.ts`
        // has some tests for this.
        if is_str_or_union(&orig) && casted.is_str() {
            return Ok(());
        }

        match orig.normalize() {
            Type::Union(ref rt) => {
                let castable = rt.types.iter().any(|v| casted.type_eq(v));

                if castable {
                    return Ok(());
                }
            }

            _ => {}
        }

        match casted.normalize() {
            Type::Tuple(ref lt) => {
                //
                match orig.normalize() {
                    Type::Tuple(ref rt) => {
                        //
                        if lt.elems.len() != rt.elems.len() {
                            Err(Error::InvalidTupleCast {
                                span,
                                left: lt.span(),
                                right: rt.span(),
                            })?;
                        }

                        let mut all_castable = true;
                        //
                        for (i, left_element) in lt.elems.iter().enumerate() {
                            // if rt.types.len() >= i {
                            //     all_castable = false;
                            //     break;
                            // }
                            let right_element = &rt.elems[i];

                            let res = self.validate_type_cast_inner(span, &right_element.ty, &left_element.ty);

                            if res.is_err() {
                                all_castable = false;
                                break;
                            }
                        }

                        if all_castable {
                            return Ok(());
                        }
                    }

                    _ => {}
                }
            }

            Type::Array(ref lt) => {
                //
                match orig.normalize() {
                    Type::Tuple(ref rt) => {
                        if rt.elems[0].ty.type_eq(&lt.elem_type) {
                            return Ok(());
                        }
                    }

                    // fallback to .assign
                    _ => {}
                }
            }

            // fallback to .assign
            _ => {}
        }

        // self.assign(&casted_ty, &orig_ty, span)?;

        match casted.normalize() {
            Type::Tuple(ref rt) => {
                //
                match orig.normalize() {
                    Type::Tuple(ref lt) => {}
                    _ => {}
                }
            }

            _ => {}
        }

        // interface P {}
        // interface C extends P {}
        //
        // declare var c:C
        // declare var p:P
        //
        // console.log(c as C)
        // console.log(c as P)
        // console.log(p as C)
        // console.log(p as P)
        //
        // We can cast P to C
        if let Some(true) = self.extends(span, orig, casted, Default::default()) {
            return Ok(());
        }
        if let Some(true) = self.extends(span, casted, orig, Default::default()) {
            return Ok(());
        }

        self.castable(span, &orig, &casted, Default::default())
            .and_then(|castable| {
                if castable {
                    Ok(())
                } else {
                    Err(Error::NonOverlappingTypeCast { span })
                }
            })
            .convert_err(|err| Error::NonOverlappingTypeCast { span })
    }

    pub(crate) fn has_overlap(&mut self, span: Span, l: &Type, r: &Type, opts: CastableOpts) -> VResult<bool> {
        let l = l.normalize();
        let r = r.normalize();

        if l.type_eq(r) {
            return Ok(true);
        }

        Ok(self.castable(span, l, r, opts)? || self.castable(span, r, l, opts)?)
    }

    /// # Parameters
    ///
    /// - `l`: from
    /// - `r`: to

    pub(crate) fn castable(&mut self, span: Span, from: &Type, to: &Type, opts: CastableOpts) -> VResult<bool> {
        let from = from.normalize();
        let to = to.normalize();

        // Overlaps with all types.
        if from.is_any() || from.is_kwd(TsKeywordTypeKind::TsNullKeyword) || from.is_kwd(TsKeywordTypeKind::TsUndefinedKeyword) {
            return Ok(true);
        }

        if from.type_eq(to) {
            return Ok(true);
        }

        match (from, to) {
            (
                Type::Lit(LitType {
                    lit: RTsLit::Number(..), ..
                }),
                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsNumberKeyword,
                    ..
                }),
            ) => return Ok(true),
            (
                Type::Lit(LitType { lit: RTsLit::Str(..), .. }),
                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsStringKeyword,
                    ..
                }),
            ) => return Ok(true),
            (
                Type::Lit(LitType { lit: RTsLit::Bool(..), .. }),
                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsBooleanKeyword,
                    ..
                }),
            ) => return Ok(true),
            (
                Type::Lit(LitType {
                    lit: RTsLit::BigInt(..), ..
                }),
                Type::Keyword(KeywordType {
                    kind: TsKeywordTypeKind::TsBigIntKeyword,
                    ..
                }),
            ) => return Ok(true),
            (
                Type::Lit(LitType {
                    lit: RTsLit::Number(..), ..
                }),
                Type::Lit(LitType {
                    lit: RTsLit::Number(..), ..
                }),
            )
            | (Type::Lit(LitType { lit: RTsLit::Str(..), .. }), Type::Lit(LitType { lit: RTsLit::Str(..), .. }))
            | (
                Type::Lit(LitType {
                    lit: RTsLit::BigInt(..), ..
                }),
                Type::Lit(LitType {
                    lit: RTsLit::BigInt(..), ..
                }),
            ) => return Ok(false),

            (Type::Function(..), Type::Interface(Interface { name, .. })) if name == "Function" => return Ok(true),
            _ => {}
        }

        // TODO(kdy1): More check
        if from.is_fn_type() && to.is_fn_type() {
            return Ok(false);
        }

        match (from, to) {
            (Type::Ref(_), _) => {
                let from = self.expand_top_ref(span, Cow::Borrowed(from), Default::default())?.freezed();
                return self.castable(span, &from, to, opts);
            }
            (_, Type::Ref(_)) => {
                let to = self.expand_top_ref(span, Cow::Borrowed(to), Default::default())?.freezed();
                return self.castable(span, from, &to, opts);
            }

            (Type::TypeLit(lt), Type::TypeLit(rt)) => {
                // It's an error if type of the parameter of index signature is same but type
                // annotation is different.
                for lm in &lt.members {
                    for rm in &rt.members {
                        match (lm, rm) {
                            (TypeElement::Index(lm), TypeElement::Index(rm)) => {
                                if lm.params.type_eq(&rm.params) {
                                    if let Some(lt) = &lm.type_ann {
                                        if let Some(rt) = &rm.type_ann {
                                            if self.assign(span, &mut Default::default(), &lt, &rt).is_err()
                                                && self.assign(span, &mut Default::default(), &rt, &lt).is_err()
                                            {
                                                return Ok(false);
                                            }
                                        }
                                    }
                                }
                            }

                            _ => {}
                        }
                    }
                }
            }

            _ => {}
        }

        // TODO(kdy1): This is wrong
        if from.is_type_lit() && to.is_type_lit() {
            return Ok(true);
        }

        match from {
            Type::Union(l) => {
                for l in &l.types {
                    if self.castable(span, l, to, opts)? {
                        return Ok(true);
                    }
                }

                return Ok(false);
            }
            _ => {}
        }

        match to {
            Type::Union(to) => {
                for to in &to.types {
                    if self.castable(span, from, &to, opts)? {
                        return Ok(true);
                    }
                }

                return Ok(false);
            }

            Type::Intersection(to) => {
                for to in &to.types {
                    if self.castable(span, from, &to, opts)? {
                        return Ok(true);
                    }
                }

                return Ok(false);
            }
            _ => {}
        }

        if from.is_num() {
            if self.can_be_casted_to_number_in_rhs(span, &to) {
                return Ok(true);
            }
        }

        if from.is_class() && (to.is_interface() || to.is_type_lit()) {
            return Ok(false);
        }

        // class A {}
        // class B extends A {}
        //
        // We can cast A to B, thus from = A, to = B.
        if let Ok(()) = self.assign_with_opts(
            &mut Default::default(),
            AssignOpts {
                span,
                disallow_different_classes: opts.disallow_different_classes,
                allow_assignment_to_param_constraint: opts.allow_assignment_to_param_constraint,
                disallow_special_assignment_to_empty_class: opts.disallow_special_assignment_to_empty_class,
                for_castablity: true,
                ..Default::default()
            },
            from,
            to,
        ) {
            return Ok(true);
        }

        Ok(false)
    }
}
