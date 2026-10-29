use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, format_ident};
use syn::{parse_macro_input, FnArg, ImplItem, ItemFn, ItemImpl, ItemStruct, Pat, ReturnType, Type};

// wraps a free fn as a plugin ABI export
#[proc_macro_attribute]
pub fn plugin_fn(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let user_vis = &input.vis;
    let user_name = input.sig.ident.clone();
    let user_inputs = input.sig.inputs.clone();
    let user_output = input.sig.output.clone();
    let user_block = input.block.clone();

    // the wrapper takes the original name and the user fn moves to `__edge_impl_<name>`
    let impl_name = syn::Ident::new(
        &format!("__edge_impl_{}", user_name),
        proc_macro2::Span::call_site(),
    );

    let mut bindings: Vec<(syn::Ident, syn::Type)> = Vec::new();
    for (i, arg) in user_inputs.iter().enumerate() {
        match arg {
            FnArg::Typed(pat) => {
                let name = match &*pat.pat {
                    Pat::Ident(id) => id.ident.clone(),
                    _ => syn::Ident::new(&format!("__arg{}", i), proc_macro2::Span::call_site()),
                };
                bindings.push((name, (*pat.ty).clone()));
            }
            FnArg::Receiver(_) => {
                return TokenStream::from(quote! {
                    compile_error!("#[plugin_fn] takes no receiver, write methods under #[plugin_methods]");
                });
            }
        }
    }

    let return_ty: syn::Type = match &user_output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, t) => (**t).clone(),
    };
    let is_result = matches!(&return_ty, syn::Type::Path(p) if p.path.segments.last().map(|s| s.ident == "Result").unwrap_or(false));

    // the host always appends a kwargs slot after the fixed positionals and an optional `Args`
    let type_named = |ty: &syn::Type, n: &str| matches!(ty, Type::Path(p) if p.path.segments.last().map(|s| s.ident == n).unwrap_or(false));
    let last_is_kwargs = bindings.last().map(|(_, ty)| type_named(ty, "Kwargs")).unwrap_or(false);
    let args_idx = bindings.iter().position(|(_, ty)| type_named(ty, "Args"));
    let has_args = args_idx.is_some();
    let num_fixed = bindings.len() - (has_args as usize) - (last_is_kwargs as usize);

    if args_idx.is_some_and(|idx| idx != num_fixed) {
        return TokenStream::from(quote! { compile_error!("#[plugin_fn] `Args` must follow the fixed params, before any `Kwargs`"); });
    }
    if bindings.iter().take(bindings.len().saturating_sub(1)).any(|(_, ty)| type_named(ty, "Kwargs")) {
        return TokenStream::from(quote! { compile_error!("#[plugin_fn] `Kwargs` must be the final parameter"); });
    }

    let decodes: Vec<TokenStream2> = bindings.iter().enumerate().map(|(i, (name, ty))| {
        if type_named(ty, "Kwargs") {
            // the kwargs handle is always the last argv slot
            quote! {
                let h = unsafe { *argv.add((argc as usize) - 1) };
                let #name: #ty = match <#ty as ::plugin::FromValue>::from_handle(h) {
                    Ok(v) => v,
                    Err(e) => { ::plugin::__internals::stash_error(e); return 1; }
                };
            }
        } else if type_named(ty, "Args") {
            // takes every positional past the fixed params but not the trailing kwargs slot
            quote! {
                let mut __args: ::alloc::vec::Vec<::plugin::Handle> = ::alloc::vec::Vec::new();
                let mut __k = #num_fixed;
                let __pos_end = (argc as usize) - 1;
                while __k < __pos_end {
                    __args.push(::plugin::Handle::borrow(unsafe { *argv.add(__k) }));
                    __k += 1;
                }
                let #name: ::plugin::Args = ::plugin::Args(__args);
            }
        } else {
            quote! {
                let h = unsafe { *argv.add(#i) };
                let #name: #ty = match <#ty as ::plugin::FromValue>::from_handle(h) {
                    Ok(v) => v,
                    Err(e) => { ::plugin::__internals::stash_error(e); return 1; }
                };
            }
        }
    }).collect();
    let arg_names: Vec<&syn::Ident> = bindings.iter().map(|(n, _)| n).collect();

    // with `Args` the positional count is a lower bound and otherwise exact
    let argc_check = if has_args {
        quote! {
            if (argc as usize) < #num_fixed + 1 {
                ::plugin::__internals::stash_error(::plugin::Error::Type(::alloc::format!("{} expects at least {} positional args, got {}", stringify!(#user_name), #num_fixed, (argc as usize).saturating_sub(1))));
                return 1;
            }
        }
    } else {
        quote! {
            if (argc as usize) != #num_fixed + 1 {
                ::plugin::__internals::stash_error(::plugin::Error::Type(::alloc::format!("{} expects {} positional args, got {}", stringify!(#user_name), #num_fixed, (argc as usize).saturating_sub(1))));
                return 1;
            }
        }
    };

    let invoke = if is_result {
        quote! {
            match #impl_name(#(#arg_names),*) {
                Ok(v) => v,
                Err(e) => return ::plugin::__internals::fail(e),
            }
        }
    } else {
        quote! { #impl_name(#(#arg_names),*) }
    };

    // free fns export under `__fn_` so no wasm symbol collides with C or build-std ones
    let name_str = user_name.to_string();
    let export_name: String = if name_str.starts_with("__const_") || name_str.starts_with("__class_") {
        name_str
    } else {
        format!("__fn_{}", name_str)
    };

    let expanded = quote! {
        #[doc(hidden)]
        #user_vis fn #impl_name(#user_inputs) #user_output #user_block

        #[doc(hidden)]
        #[unsafe(export_name = #export_name)]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn #user_name(argv: *const u32, argc: u32, out: *mut u32) -> i32 {
            #argc_check
            #(#decodes)*
            let __value = { #invoke };
            // a full path so user code never has to import `IntoValue`
            match ::plugin::IntoValue::into_handle(__value) {
                Ok(h) => {
                    unsafe { *out = h.into_raw(); }
                    0
                }
                Err(e) => ::plugin::__internals::fail(e),
            }
        }
    };

    expanded.into()
}

// exports the fn a waiting system call settles into as `__edge_resume`
#[proc_macro_attribute]
pub fn plugin_resume(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let user_name = input.sig.ident.clone();
    let expanded = quote! {
        #input

        #[doc(hidden)]
        #[unsafe(export_name = "__edge_resume")]
        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        pub extern "C" fn __edge_resume(call: u32, answer: u32, out: *mut u32) -> i32 {
            // a zero handle means the call failed and the host holds its error
            let answer = match answer {
                0 => Err(::plugin::last_error()),
                h => Ok(::plugin::Handle::from_raw(h)),
            };
            let value = match #user_name(call, answer) {
                Ok(v) => v,
                Err(e) => return ::plugin::__internals::fail(e),
            };
            match ::plugin::IntoValue::into_handle(value) {
                Ok(h) => {
                    unsafe { *out = h.into_raw(); }
                    0
                }
                Err(e) => ::plugin::__internals::fail(e),
            }
        }
    };
    expanded.into()
}

// a zero-arg fn as a module constant exported as `__const_<name>`
#[proc_macro_attribute]
pub fn plugin_const(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as ItemFn);
    if !input.sig.inputs.is_empty() {
        return TokenStream::from(quote! { compile_error!("#[plugin_const] takes no parameters"); });
    }
    input.sig.ident = format_ident!("__const_{}", input.sig.ident);
    plugin_fn(TokenStream::new(), TokenStream::from(quote!(#input)))
}

// the per-class state as a static map and an id counter
#[proc_macro_attribute]
pub fn plugin_class(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);
    let name = input.ident.clone();
    let upper = name.to_string().to_uppercase();
    let state_ident = format_ident!("__{}_STATE", upper);
    let id_ident = format_ident!("__{}_NEXT_ID", upper);

    let expanded = quote! {
        #input

        #[doc(hidden)]
        static #state_ident: ::plugin::PluginCell<::alloc::collections::BTreeMap<i64, #name>> = ::plugin::PluginCell::new();
        #[doc(hidden)]
        static #id_ident: ::core::sync::atomic::AtomicI64 = ::core::sync::atomic::AtomicI64::new(1);

        impl #name {
            #[doc(hidden)]
            pub fn __edge_alloc_id() -> i64 {
                #id_ident.fetch_add(1, ::core::sync::atomic::Ordering::Relaxed)
            }

            #[doc(hidden)]
            pub fn __edge_state() -> &'static mut ::alloc::collections::BTreeMap<i64, #name> {
                #state_ident.get_or_init(::alloc::collections::BTreeMap::new)
            }

            #[doc(hidden)]
            pub fn __edge_insert(value: #name) -> i64 {
                let id = Self::__edge_alloc_id();
                Self::__edge_state().insert(id, value);
                id
            }
        }
    };
    expanded.into()
}

// marks the constructor inside a `#[plugin_methods]` impl
#[proc_macro_attribute]
pub fn plugin_ctor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

// lowers an impl block into `__class_<Name>_<method>` exports
#[proc_macro_attribute]
pub fn plugin_methods(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemImpl);
    let self_ty = match &*input.self_ty {
        Type::Path(p) => p.path.segments.last().expect("plugin_methods: empty path").ident.clone(),
        _ => return quote! { compile_error!("#[plugin_methods] requires a simple type path"); }.into(),
    };
    let class_name = self_ty.to_string();

    let mut emitted: Vec<TokenStream2> = Vec::new();

    for item in &input.items {
        let ImplItem::Fn(method) = item else { continue; };
        let is_ctor = method.attrs.iter().any(|a| a.path().is_ident("plugin_ctor"));
        let method_name = method.sig.ident.to_string();
        let method_ident = &method.sig.ident;

        if is_ctor {
            let export_ident = format_ident!("__class_{}___init__", class_name);
            emitted.push(quote! {
                #[::plugin::plugin_fn]
                fn #export_ident(self_h: ::plugin::Handle) -> ::plugin::Result<()> {
                    let instance = #self_ty::#method_ident();
                    let id = #self_ty::__edge_insert(instance);
                    let id_handle = <i64 as ::plugin::IntoValue>::into_handle(id)?;
                    self_h.set_attr("__rust_id", &id_handle)?;
                    Ok(())
                }
            });
            continue;
        }

        let export_ident = format_ident!("__class_{}_{}", class_name, method_name);
        let user_args: Vec<_> = method.sig.inputs.iter().skip(1).collect();
        let user_arg_names: Vec<_> = user_args.iter().filter_map(|a| match a {
            FnArg::Typed(pt) => match &*pt.pat { Pat::Ident(id) => Some(id.ident.clone()), _ => None },
            _ => None,
        }).collect();
        let user_arg_types: Vec<_> = user_args.iter().filter_map(|a| match a {
            FnArg::Typed(pt) => Some((*pt.ty).clone()),
            _ => None,
        }).collect();
        let user_ret_ty: syn::Type = match &method.sig.output {
            ReturnType::Default => syn::parse_quote!(()),
            ReturnType::Type(_, t) => (**t).clone(),
        };
        // a method that already returns `Result` is not wrapped in another
        let is_user_result = matches!(&user_ret_ty, Type::Path(p)
            if p.path.segments.last().map(|s| s.ident == "Result").unwrap_or(false));
        let (wrapper_ret, call_expr) = if is_user_result {
            (quote! { #user_ret_ty },
             quote! { instance.#method_ident(#(#user_arg_names),*) })
        } else {
            (quote! { ::plugin::Result<#user_ret_ty> },
             quote! { Ok(instance.#method_ident(#(#user_arg_names),*)) })
        };

        emitted.push(quote! {
            #[::plugin::plugin_fn]
            fn #export_ident(self_h: ::plugin::Handle #(, #user_arg_names: #user_arg_types)*) -> #wrapper_ret {
                let id_handle = self_h.get_attr("__rust_id")?;
                let id = <i64 as ::plugin::FromValue>::from_handle(id_handle.raw())?;
                let state = #self_ty::__edge_state();
                let instance = state.get_mut(&id).ok_or_else(|| ::plugin::Error::Runtime(::alloc::string::String::from("instance state missing")))?;
                #call_expr
            }
        });
    }

    let expanded = quote! {
        #input
        #(#emitted)*
    };
    expanded.into()
}
