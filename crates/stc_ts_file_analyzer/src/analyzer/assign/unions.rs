use std::borrow::Cow;

use itertools::Itertools;
use stc_ts_ast_rnode::RBool;
use stc_ts_errors::{DebugExt, Error};
use stc_ts_type_ops::Fix;
use stc_ts_types::{
    KeywordType, LitType, LitTypeMetadata, PropertySignature, Tuple, TupleElement, Type, TypeElement, TypeLit, Union, UnionMetadata,
};
use stc_utils::cache::{Freeze, ALLOW_DEEP_CLONE};
use swc_common::{Span, DUMMY_SP};
use swc_ecma_ast::TsKeywordTypeKind;

use crate::{
    analyzer::{
        assign::{AssignData, AssignOpts},
        Analyzer,
    },
    VResult,
};

impl Analyzer<'_, '_> {
    /// # Cases
    ///
    /// Cases handled by this methods are
    ///
    ///  - lhs = `(["a", number] | ["b", number] | ["c", string]);`
    ///  - rhs = `[("b" | "a"), 1];`
    pub(super) fn assign_to_union(&mut self, data: &mut AssignData, l: &Type, r: &Type, opts: AssignOpts) -> Option<VResult<()>> {
        let r_res = self.flatten_unions_for_assignment(opts.span, Cow::Borrowed(r));

        match r_res {
            Ok(mut r) => {
                r.make_clone_cheap();

                if r.is_union_type() {
                    Some(
                        self.assign_with_opts(data, opts, l, &r)
                            .context("tried to assign to a flattened union to another union"),
                    )
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    }

    fn flatten_unions_for_assignment(&mut self, span: Span, ty: Cow<Type>) -> VResult<Type> {
        let ty = self.normalize(Some(span), ty, Default::default())?;

        match ty.normalize() {
            Type::Tuple(ty) => {
                let mut tuple = Type::Tuple(Tuple {
                    elems: Default::default(),
                    ..*ty
                });

                for el in &ty.elems {
                    self.append_tuple_element_to_type(span, &mut tuple, el)
                        .context("tried to append an element to a type")?;
                }

                Ok(tuple)
            }
            Type::TypeLit(ty) => {
                let mut type_lit = Type::TypeLit(TypeLit {
                    members: Default::default(),
                    ..*ty
                });

                for el in &ty.members {
                    self.append_type_element_to_type(span, &mut type_lit, el)
                        .context("tried to append an element to a type")?;
                }

                Ok(type_lit)
            }
            _ => Ok(ty.into_owned()),
        }
    }

    /// TODO(kdy1): Use Cow<TupleElement>
    fn append_type_element_to_type(&mut self, span: Span, to: &mut Type, el: &TypeElement) -> VResult<()> {
        match el {
            TypeElement::Property(el) => {
                if let Some(el_ty) = &el.type_ann {
                    if let Some(ty) = self.expand_union_for_assignment(span, &el_ty) {
                        let mut to_types = (0..ty.types.len()).map(|_| ALLOW_DEEP_CLONE.set(&(), || to.clone())).collect_vec();

                        for (idx, el_ty) in ty.types.iter().enumerate() {
                            self.append_type_element_to_type(
                                span,
                                &mut to_types[idx],
                                &TypeElement::Property(PropertySignature {
                                    type_ann: Some(box el_ty.clone()),
                                    ..el.clone()
                                }),
                            )?;
                        }

                        *to = Type::Union(Union {
                            span: ty.span,
                            types: to_types,
                            metadata: ty.metadata,
                        })
                        .fixed();

                        return Ok(());
                    }
                }
            }

            _ => {}
        }

        match to.normalize_mut() {
            Type::Union(to) => {
                for to in &mut to.types {
                    self.append_type_element_to_type(span, to, el)?;
                }

                Ok(())
            }
            Type::TypeLit(to) => {
                to.members.push(el.clone());

                Ok(())
            }
            _ => Err(Error::SimpleAssignFailed { span, cause: None }),
        }
    }

    /// TODO(kdy1): Use Cow<TupleElement>
    fn append_tuple_element_to_type(&mut self, span: Span, to: &mut Type, el: &TupleElement) -> VResult<()> {
        if let Some(el_ty) = self.expand_union_for_assignment(span, &el.ty) {
            let mut to_types = (0..el_ty.types.len()).map(|_| to.clone()).collect_vec();

            for (idx, el_ty) in el_ty.types.iter().enumerate() {
                self.append_tuple_element_to_type(
                    span,
                    &mut to_types[idx],
                    &TupleElement {
                        span: el.span,
                        label: el.label.clone(),
                        ty: box el_ty.clone(),
                    },
                )?;
            }

            *to = Type::Union(Union {
                span: el_ty.span,
                types: to_types,
                metadata: el_ty.metadata,
            });

            return Ok(());
        }

        match to.normalize_mut() {
            Type::Union(to) => {
                for to in &mut to.types {
                    self.append_tuple_element_to_type(span, to, el)?;
                }

                Ok(())
            }
            Type::Tuple(to) => {
                to.elems.push(el.clone());

                Ok(())
            }
            _ => Err(Error::SimpleAssignFailed { span, cause: None }),
        }
    }

    /// Expands `boolean` to `true | false`.
    fn expand_union_for_assignment<'a>(&mut self, span: Span, t: &'a Type) -> Option<Union> {
        let t = self.normalize(Some(span), Cow::Borrowed(t), Default::default()).ok()?;

        match t.normalize() {
            Type::Keyword(KeywordType {
                span,
                metadata,
                kind: TsKeywordTypeKind::TsBooleanKeyword,
                ..
            }) => Some(Union {
                span: *span,
                types: vec![
                    Type::Lit(LitType {
                        span: DUMMY_SP,
                        lit: stc_ts_ast_rnode::RTsLit::Bool(RBool {
                            span: DUMMY_SP,
                            value: true,
                        }),
                        metadata: LitTypeMetadata::default(),
                    }),
                    Type::Lit(LitType {
                        span: DUMMY_SP,
                        lit: stc_ts_ast_rnode::RTsLit::Bool(RBool {
                            span: DUMMY_SP,
                            value: false,
                        }),
                        metadata: LitTypeMetadata::default(),
                    }),
                ],
                metadata: UnionMetadata {
                    common: metadata.common,
                    ..Default::default()
                },
            }),
            Type::Union(ty) => Some(ty.clone()),
            _ => None,
        }
    }
}
