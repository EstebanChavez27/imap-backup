// Copyright (C) 2026 Esteban Chávez / Contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Token de cancelación compartido entre la UI (o el usuario de la librería) y los
/// hilos de trabajo. Se consulta en todos los bucles largos, de modo que un backup o
/// una restauración se pueden interrumpir sin cerrar la aplicación y sin perder el
/// progreso, porque el manifiesto queda escrito en disco.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelToken({})", self.is_cancelled())
    }
}

/// Error de cancelación. Se distingue de un error real para que el resumen final
/// pueda informar "cancelado por el usuario" en lugar de "falló".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Operación cancelada por el usuario")
    }
}

impl std::error::Error for Cancelled {}

/// Devuelve `Err(Cancelled)` si el usuario pidió detener la operación.
pub fn check(cancel: &CancelToken) -> anyhow::Result<()> {
    if cancel.is_cancelled() {
        Err(anyhow::Error::new(Cancelled))
    } else {
        Ok(())
    }
}

/// Permite reconocer el error de cancelación dentro de una cadena de `anyhow`.
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Cancelled>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_is_shared_between_clones() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }

    #[test]
    fn cancellation_error_is_detected_through_context() {
        let token = CancelToken::new();
        token.cancel();
        let err = check(&token).context("paso 1").unwrap_err();
        assert!(is_cancelled(&err));
        assert!(!is_cancelled(&anyhow::anyhow!("otro error")));
    }

    use anyhow::Context;
}
