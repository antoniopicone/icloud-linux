//! A pretend system, for `--demo`: try the whole wizard without an Apple
//! account and without changing anything on the machine.

use std::{path::Path, thread, time::Duration};

use icloud_api::AccountInfo;
use icloud_core::{
    Result,
    i18n::{tr, trf},
    installer::{Backend, InstallPlan, Prepared, StepOutcome},
    setup::{Check, Severity},
};

#[derive(Debug, Default)]
pub(crate) struct DemoBackend;

impl Backend for DemoBackend {
    fn preflight(&self) -> Vec<Check> {
        vec![
            Check { severity: Severity::Ok, message: tr("/dev/fuse is present").into(), fix: None },
            Check { severity: Severity::Ok, message: trf("{} found", &[&"/usr/bin/fusermount3"]), fix: None },
            Check { severity: Severity::Ok, message: trf("daemon: {}", &[&"/usr/local/bin/icloudd"]), fix: None },
        ]
    }

    fn prepare(&self, plan: &InstallPlan) -> Result<Prepared> {
        thread::sleep(Duration::from_millis(400));
        Ok(Prepared { mount_dir: plan.mount_dir.clone(), used_fallback: false })
    }

    fn sign_in_with_browser(&self) -> Result<AccountInfo> {
        // Long enough to see the waiting message.
        thread::sleep(Duration::from_millis(2500));
        Ok(AccountInfo { apple_id: Some("demo@icloud.com".into()), full_name: Some("Demo User".into()) })
    }

    fn finish(&self, plan: &InstallPlan, mount_dir: &Path) -> Vec<StepOutcome> {
        let step = |title: &str, detail: String| {
            thread::sleep(Duration::from_millis(700));
            StepOutcome { title: title.to_owned(), result: Ok(detail) }
        };
        let mut steps = vec![
            step(tr("Start the iCloud service"), tr("running").into()),
            step(tr("Mount iCloud Drive"), mount_dir.display().to_string()),
        ];
        if plan.keep_indexer_out {
            steps.push(step(tr("Keep the search indexer out"), tr("done").into()));
        }
        if plan.show_sidebar_status {
            steps.push(step(tr("Show activity in the Files sidebar"), tr("appears next to iCloud").into()));
        }
        if plan.add_context_menu {
            steps.push(step(
                tr("Add \"Download from iCloud\" to the right-click menu"),
                tr("right-click a file, then Scripts").into(),
            ));
        }
        steps
    }
}
