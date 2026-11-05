use cantrip_engine::{ipc, keys};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum Operation {
    Check,
    Save,
    Remove,
    Approve,
}

struct Completion {
    access: ipc::KeychainAuthorization,
    saved: bool,
    problem: Option<&'static str>,
}

pub(crate) struct Credential {
    id: String,
    access: ipc::KeychainAuthorization,
    saved: bool,
    pending: Option<Receiver<Completion>>,
    cancel: Arc<AtomicBool>,
    problem: Option<&'static str>,
}

impl Default for Credential {
    fn default() -> Self {
        Self {
            id: String::new(),
            access: ipc::KeychainAuthorization::NotSet,
            saved: false,
            pending: None,
            cancel: Arc::new(AtomicBool::new(false)),
            problem: None,
        }
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Credential {
    pub(crate) fn select(&mut self, id: &str) {
        let id = id.trim();
        if self.id == id {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.pending = None;
        self.id = id.to_owned();
        self.saved = false;
        self.problem = None;
        if id.is_empty() {
            self.access = ipc::KeychainAuthorization::NotSet;
        } else {
            self.refresh();
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn access(&self) -> ipc::KeychainAuthorization {
        self.access
    }

    pub(crate) fn label(&self) -> &'static str {
        if self.saved && self.access == ipc::KeychainAuthorization::Ready {
            "Saved"
        } else {
            self.access.label()
        }
    }

    pub(crate) fn problem(&self) -> Option<&'static str> {
        self.problem
    }

    pub(crate) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn refresh(&mut self) {
        self.start(Operation::Check, None);
    }

    pub(crate) fn save(&mut self, secret: String) {
        self.start(Operation::Save, Some(secret));
    }

    pub(crate) fn remove(&mut self) {
        self.start(Operation::Remove, None);
    }

    pub(crate) fn approve(&mut self) {
        self.start(Operation::Approve, None);
    }

    pub(crate) fn poll(&mut self) -> bool {
        let result = match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => Completion {
                access: ipc::KeychainAuthorization::Unavailable,
                saved: false,
                problem: Some("Keyring operation could not finish. Retry."),
            },
            _ => return false,
        };
        self.pending = None;
        self.access = result.access;
        self.saved = result.saved;
        self.problem = result.problem;
        true
    }

    fn start(&mut self, operation: Operation, secret: Option<String>) {
        if self.busy() || self.id.is_empty() {
            return;
        }
        let previous_access = self.access;
        let previously_saved = self.saved;
        self.problem = None;
        self.access = if matches!(operation, Operation::Approve) {
            ipc::KeychainAuthorization::Authorizing
        } else {
            ipc::KeychainAuthorization::Checking
        };
        let id = self.id.clone();
        let cancel = self.cancel.clone();
        let (send, receive) = mpsc::channel();
        self.pending = Some(receive);
        std::thread::spawn(move || {
            let result = match operation {
                Operation::Check => keys::get(&id).map(drop),
                Operation::Save => {
                    let mut secret = secret.expect("saving carries only the new key");
                    let result = keys::set(&id, &secret);
                    secret.clear();
                    result
                }
                Operation::Remove => keys::delete(&id),
                Operation::Approve => keys::authorize(&id, &cancel),
            };
            let ok = result.is_ok();
            let mutation_failed = !ok && matches!(operation, Operation::Save | Operation::Remove);
            let access = if mutation_failed {
                previous_access
            } else if ok && matches!(operation, Operation::Remove) {
                ipc::KeychainAuthorization::NotSet
            } else {
                ipc::KeychainAuthorization::from_result(result)
            };
            let problem = if mutation_failed {
                Some(match operation {
                    Operation::Save => "Key was not saved. Unlock the keyring and paste a nonempty key without whitespace.",
                    Operation::Remove => "Key was not removed. Unlock the keyring and retry.",
                    Operation::Check | Operation::Approve => unreachable!("only mutations can fail here"),
                })
            } else {
                None
            };
            let _ = send.send(Completion {
                access,
                saved: if mutation_failed {
                    previously_saved
                } else {
                    ok && matches!(operation, Operation::Save)
                },
                problem,
            });
            if ok && !matches!(operation, Operation::Check) {
                let _ = ipc::command(ipc::Command::Reload);
            }
        });
    }
}
