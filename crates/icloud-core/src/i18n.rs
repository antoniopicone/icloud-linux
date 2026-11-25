//! Translations of user-facing text.
//!
//! The English text in the code is the source and the key: `tr("Continue")`
//! returns "Continua" when the user's language is Italian and "Continue"
//! otherwise. Text with values in it uses `{}` placeholders, filled in order by
//! [`trf`]. A text with no translation is shown in English.
//!
//! The language is read once from the environment, the way gettext does:
//! `LANGUAGE` (a priority list such as `it:en`) unless the locale is `C`, then
//! `LC_ALL`, `LC_MESSAGES` and `LANG`.
//!
//! Adding a language: a new [`Lang`] variant, its catalog next to [`ITALIAN`],
//! and a line in [`Lang::from_code`]. The tests check that every `tr`/`trf`
//! text in the installer has a translation and that placeholders match.

use std::{collections::HashMap, fmt::Display, sync::OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    English,
    Italian,
}

impl Lang {
    /// The language for a locale or language name such as `it_IT.UTF-8`,
    /// `it` or `en_GB`, if it is one with a translation.
    fn from_code(code: &str) -> Option<Self> {
        let language = code.split(['_', '.', '@', '-']).next().unwrap_or_default();
        match language.to_ascii_lowercase().as_str() {
            "it" => Some(Self::Italian),
            "en" => Some(Self::English),
            _ => None,
        }
    }

    fn catalog(self) -> Option<&'static HashMap<&'static str, &'static str>> {
        static ITALIAN_MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
        match self {
            Self::English => None,
            Self::Italian => Some(ITALIAN_MAP.get_or_init(|| ITALIAN.iter().copied().collect())),
        }
    }
}

/// The user's language. Always English in this crate's own tests, so their
/// expectations do not depend on the machine running them.
pub fn lang() -> Lang {
    if cfg!(test) {
        return Lang::English;
    }
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(|| detect(&|name| std::env::var(name).ok()))
}

fn detect(var: &dyn Fn(&str) -> Option<String>) -> Lang {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    let Some(locale) = set("LC_ALL").or_else(|| set("LC_MESSAGES")).or_else(|| set("LANG")) else {
        return Lang::English;
    };
    // As in gettext, LANGUAGE is ignored when the locale is "C".
    if locale == "C" || locale == "POSIX" || locale.starts_with("C.") {
        return Lang::English;
    }
    let wanted = set("LANGUAGE").unwrap_or(locale);
    wanted.split(':').find_map(Lang::from_code).unwrap_or(Lang::English)
}

/// `english` in the user's language.
pub fn tr(english: &'static str) -> &'static str {
    tr_in(lang(), english)
}

/// `english` in `lang`.
pub fn tr_in(lang: Lang, english: &'static str) -> &'static str {
    lang.catalog().and_then(|catalog| catalog.get(english).copied()).unwrap_or(english)
}

/// Like [`tr`], for text that is only known at run time (a message built
/// elsewhere). Returns it unchanged when there is no translation.
pub fn tr_text(english: &str) -> String {
    lang().catalog().and_then(|catalog| catalog.get(english).copied()).unwrap_or(english).to_owned()
}

/// `english` in the user's language with its `{}` placeholders filled by
/// `args`, in order.
pub fn trf(english: &'static str, args: &[&dyn Display]) -> String {
    fill(tr(english), args)
}

fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut rest = template;
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        if let Some(arg) = args.next() {
            out.push_str(&arg.to_string());
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// English → Italian. Apple's own names are kept as Apple's Italian pages
/// show them ("Considera attendibile"); the Files menu entry keeps its real,
/// English name.
const ITALIAN: &[(&str, &str)] = &[
    // installer: window and steps
    ("iCloud for Linux", "iCloud per Linux"),
    ("iCloud for Linux — demo", "iCloud per Linux — demo"),
    ("Step {} of {} · {}", "Passo {} di {} · {}"),
    ("Requirements", "Requisiti"),
    ("Folder", "Cartella"),
    ("Sign in", "Accesso"),
    ("Setting up", "Configurazione"),
    ("Done", "Fine"),
    ("Quit", "Esci"),
    ("Check Again", "Controlla di nuovo"),
    ("Continue", "Continua"),
    ("Back", "Indietro"),
    ("Close", "Chiudi"),
    // installer: welcome
    (
        "Demo mode: nothing on this computer is changed, and no browser opens: the sign-in is pretended.",
        "Modalità demo: su questo computer non cambia nulla e non si apre nessun browser, l'accesso è simulato.",
    ),
    ("Welcome to iCloud for Linux", "Ti diamo il benvenuto in iCloud per Linux"),
    (
        "This sets up iCloud Drive as a normal folder on this computer. Files download when you open them, and changes you make are uploaded for you.",
        "Configura iCloud Drive come una normale cartella su questo computer. I file si scaricano quando li apri e le modifiche che fai vengono caricate in automatico.",
    ),
    ("/dev/fuse is present", "/dev/fuse è presente"),
    ("/dev/fuse is missing", "/dev/fuse non è presente"),
    (
        "install FUSE (fuse3) and load the fuse kernel module",
        "installa FUSE (fuse3) e carica il modulo del kernel fuse",
    ),
    ("fusermount3 not found", "fusermount3 non trovato"),
    ("install the fuse3 package", "installa il pacchetto fuse3"),
    ("{} found", "{} trovato"),
    ("daemon: {}", "servizio: {}"),
    // installer: folder
    ("Where should iCloud Drive appear?", "Dove deve comparire iCloud Drive?"),
    (
        "Pick an empty folder. It will show your iCloud files.",
        "Scegli una cartella vuota: mostrerà i tuoi file di iCloud.",
    ),
    ("Choose a folder", "Scegli una cartella"),
    ("How should it work?", "Come deve funzionare?"),
    ("Open folders on demand", "Apri le cartelle quando servono"),
    ("Recommended. Fast to start, uses little disk.", "Consigliato. Parte subito e occupa poco spazio."),
    ("Index everything up front", "Indicizza tutto subito"),
    (
        "Slower to start; lets you search the whole drive.",
        "Parte più lentamente, ma permette di cercare in tutto il drive.",
    ),
    ("Extras", "Extra"),
    ("Keep the search indexer out", "Escludi l'indicizzatore della ricerca"),
    (
        "Stops desktop search from downloading the whole drive. Recommended.",
        "Evita che la ricerca del desktop scarichi tutto il drive. Consigliato.",
    ),
    ("Show activity in the Files sidebar", "Mostra l'attività nella barra laterale di File"),
    ("What iCloud is doing appears next to its name.", "Quello che sta facendo iCloud compare accanto al suo nome."),
    ("Add “Download from iCloud” to Files", "Aggiungi “Download from iCloud” a File"),
    (
        "In the right-click menu, to keep files on this computer.",
        "Nel menu del tasto destro, per tenere i file su questo computer.",
    ),
    ("Preparing the service…", "Preparazione del servizio…"),
    ("That folder could not be used; {} will be used instead.", "Quella cartella non si può usare: verrà usata {}."),
    ("Choose a folder for iCloud Drive", "Scegli una cartella per iCloud Drive"),
    ("Choose", "Scegli"),
    ("Cancel", "Annulla"),
    ("Choose a full folder path.", "Scegli il percorso completo di una cartella."),
    (
        "Choose a folder inside your home directory, not the home directory itself.",
        "Scegli una cartella dentro la tua cartella home, non la home stessa.",
    ),
    ("That path is a file, not a folder.", "Quel percorso è un file, non una cartella."),
    (
        "That folder is not empty. Pick an empty folder or a new name.",
        "Quella cartella non è vuota. Scegline una vuota o un nome nuovo.",
    ),
    // installer: sign in
    ("Sign in to iCloud", "Accedi a iCloud"),
    (
        "You sign in on Apple's own page, in a browser window that opens for this. Your Apple ID, password and verification code are typed there and never reach this app.",
        "Accedi dalla pagina di Apple, in una finestra del browser che si apre apposta. Account Apple, password e codice di verifica si inseriscono lì e non arrivano mai a questa app.",
    ),
    ("Sign In on icloud.com", "Accedi su icloud.com"),
    (
        "When Apple asks whether to trust the browser, choose Trust: that keeps you signed in here. No password is stored; when the session expires, you sign in the same way again.",
        "Quando Apple chiede se considerare attendibile il browser, scegli «Considera attendibile»: così l'accesso resta attivo qui. Nessuna password viene salvata; quando la sessione scade, accedi di nuovo allo stesso modo.",
    ),
    ("Waiting for you to sign in in the browser window…", "In attesa dell'accesso nella finestra del browser…"),
    // installer: setting up and done
    ("Setting things up", "Configurazione in corso"),
    ("Prepare the folder and the service", "Prepara la cartella e il servizio"),
    ("your account", "il tuo account"),
    ("{}, no password stored", "{}, nessuna password salvata"),
    ("Starting iCloud Drive…", "Avvio di iCloud Drive…"),
    ("Start the iCloud service", "Avvia il servizio iCloud"),
    ("running", "in esecuzione"),
    ("Mount iCloud Drive", "Monta iCloud Drive"),
    ("{} did not appear; see `icloudctl logs`", "{} non è comparsa; vedi `icloudctl logs`"),
    ("done", "fatto"),
    ("appears next to iCloud", "compare accanto a iCloud"),
    (
        "Add \"Download from iCloud\" to the right-click menu",
        "Aggiungi \"Download from iCloud\" al menu del tasto destro",
    ),
    ("right-click a file, then Scripts", "clic destro su un file, poi Script"),
    ("Open iCloud Drive", "Apri iCloud Drive"),
    ("iCloud Drive Is Ready", "iCloud Drive è pronto"),
    (
        "Your files are in {}. Folders load as you open them; a file downloads when you open it or choose “Download from iCloud” in its right-click menu. Changes you make there are uploaded automatically.",
        "I tuoi file sono in {}. Le cartelle si caricano quando le apri; un file si scarica quando lo apri o quando scegli “Download from iCloud” nel menu del tasto destro. Le modifiche che fai lì vengono caricate in automatico.",
    ),
    (
        "The Files sidebar shows what iCloud is doing next to its name.",
        "La barra laterale di File mostra accanto al suo nome quello che sta facendo iCloud.",
    ),
    (
        "Right-click a file or folder in iCloud Drive, then Scripts, to keep it on this computer.",
        "Fai clic destro su un file o una cartella in iCloud Drive, poi su Script, per tenerli su questo computer.",
    ),
    ("Almost There", "Quasi fatto"),
    (
        "Something did not finish. Run <tt>icloudctl doctor</tt> to see what is wrong and how to fix it, or <tt>icloudctl logs</tt> for details. Your sign-in was saved, so you will not have to repeat it.",
        "Qualcosa non è andato a buon fine. Esegui <tt>icloudctl doctor</tt> per vedere cosa non va e come risolverlo, oppure <tt>icloudctl logs</tt> per i dettagli. L'accesso è stato salvato: non dovrai ripeterlo.",
    ),
    // installer: command line
    (
        "usage: icloud-installer [--demo]\n\n  --demo  try the wizard without an Apple account or any change to this machine",
        "uso: icloud-installer [--demo]\n\n  --demo  prova la procedura senza un account Apple e senza modificare questo computer",
    ),
    ("unknown argument `{}`; see --help", "argomento sconosciuto `{}`; vedi --help"),
    // browser sign-in
    (
        "No Chromium-based browser was found (Chromium, Chrome, Brave, Edge, Vivaldi). Install one, or set ICLOUD_BROWSER to its path.",
        "Non è stato trovato nessun browser basato su Chromium (Chromium, Chrome, Brave, Edge, Vivaldi). Installane uno, oppure indica il suo percorso in ICLOUD_BROWSER.",
    ),
    (
        "Apple accepted the sign-in but did not say which Apple ID it is",
        "Apple ha accettato l'accesso ma non ha indicato di quale account si tratta",
    ),
    (
        "Apple did not accept the session from the browser. Try again, and choose Trust when Apple asks.",
        "Apple non ha accettato la sessione del browser. Riprova e scegli «Considera attendibile» quando Apple lo chiede.",
    ),
    ("could not start {}: {}", "impossibile avviare {}: {}"),
    ("the browser exited before it was ready", "il browser si è chiuso prima di essere pronto"),
    ("the browser did not open its debugging port in time", "il browser non ha aperto in tempo la porta di debug"),
    ("could not talk to the browser: {}", "impossibile comunicare con il browser: {}"),
    ("the browser was closed before the sign-in finished", "il browser è stato chiuso prima della fine dell'accesso"),
    ("timed out waiting for the sign-in in the browser", "tempo scaduto in attesa dell'accesso nel browser"),
    // daemon: a file that was opened but could not be downloaded
    ("Could not open “{}”", "Impossibile aprire «{}»"),
    (
        "The iCloud session has expired, so the file cannot be downloaded. Click to sign in again on icloud.com.",
        "La sessione di iCloud è scaduta, quindi il file non può essere scaricato. Fai clic per accedere di nuovo su icloud.com.",
    ),
    (
        "iCloud could not be reached. Check the connection, then open the file again.",
        "Impossibile raggiungere iCloud. Controlla la connessione, poi apri di nuovo il file.",
    ),
    ("Sign in on icloud.com", "Accedi su icloud.com"),
    // icloudctl auth --notify, after a click on that notification
    ("Signed in to iCloud", "Accesso a iCloud eseguito"),
    ("Signed in as {}. Open the file again.", "Accesso eseguito come {}. Apri di nuovo il file."),
    ("Open the file again.", "Apri di nuovo il file."),
    ("iCloud sign-in failed", "Accesso a iCloud non riuscito"),
];

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::Path};

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_language_comes_from_the_environment_like_gettext() {
        assert_eq!(detect(&env(&[("LANG", "it_IT.UTF-8")])), Lang::Italian);
        assert_eq!(detect(&env(&[("LANG", "en_US.UTF-8")])), Lang::English);
        assert_eq!(detect(&env(&[])), Lang::English);
        assert_eq!(detect(&env(&[("LANG", "it_IT.UTF-8"), ("LC_ALL", "C")])), Lang::English, "C wins");
        assert_eq!(detect(&env(&[("LANG", "it_IT.UTF-8"), ("LC_MESSAGES", "en_GB.UTF-8")])), Lang::English);
        assert_eq!(detect(&env(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "fr:it:en")])), Lang::Italian);
        assert_eq!(detect(&env(&[("LANG", "it_IT.UTF-8"), ("LANGUAGE", "en")])), Lang::English);
        assert_eq!(detect(&env(&[("LANG", "C.UTF-8"), ("LANGUAGE", "it")])), Lang::English);
        assert_eq!(detect(&env(&[("LANG", "de_DE.UTF-8")])), Lang::English, "no German: English");
        assert_eq!(detect(&env(&[("LANG", "it_CH.UTF-8")])), Lang::Italian);
    }

    #[test]
    fn lookups_fall_back_to_english_and_fill_placeholders() {
        assert_eq!(tr_in(Lang::Italian, "Continue"), "Continua");
        assert_eq!(tr_in(Lang::English, "Continue"), "Continue");
        assert_eq!(tr_in(Lang::Italian, "a text nobody translated"), "a text nobody translated");
        assert_eq!(fill(tr_in(Lang::Italian, "Step {} of {} · {}"), &[&1, &5, &"Cartella"]), "Passo 1 di 5 · Cartella");
        assert_eq!(fill("{} and {}", &[&"one"]), "one and ", "missing values leave nothing behind");
    }

    #[test]
    fn every_translation_keeps_the_placeholders_and_no_text_is_listed_twice() {
        let mut seen = HashSet::new();
        for (english, italian) in ITALIAN {
            assert!(seen.insert(english), "listed twice: {english}");
            assert_eq!(english.matches("{}").count(), italian.matches("{}").count(), "{english}");
            assert!(!italian.trim().is_empty(), "{english}");
        }
    }

    /// The text of every `tr("…")` / `trf("…", …)` call in `source`.
    fn translated_literals(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        for call in ["tr(", "trf("] {
            for (at, _) in source.match_indices(call) {
                let before = source[..at].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue; // another function whose name ends in `tr`
                }
                let rest = source[at + call.len()..].trim_start();
                let Some(body) = rest.strip_prefix('"') else { continue };
                let mut text = String::new();
                let mut chars = body.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => text.push('\n'),
                            Some('\n') => {
                                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                                    chars.next();
                                }
                            }
                            Some(other) => text.push(other),
                            None => {}
                        },
                        other => text.push(other),
                    }
                }
                found.push(text);
            }
        }
        found
    }

    #[test]
    fn every_text_the_installer_shows_has_an_italian_translation() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let files = [
            "icloud-core/src/installer/mod.rs",
            "icloud-core/src/browser.rs",
            "icloud-core/src/notify.rs",
            "icloudctl/src/commands.rs",
            "icloud-installer/src/wizard.rs",
            "icloud-installer/src/main.rs",
            "icloud-installer/src/demo.rs",
        ];
        let catalog: HashSet<&str> = ITALIAN.iter().map(|(english, _)| *english).collect();
        let mut missing = Vec::new();
        let mut total = 0;
        for file in files {
            let source = std::fs::read_to_string(crates.join(file)).unwrap();
            for text in translated_literals(&source) {
                total += 1;
                if !catalog.contains(text.as_str()) {
                    missing.push(format!("{file}: {text:?}"));
                }
            }
        }
        assert!(total > 50, "the scan found only {total} texts; is it still reading the calls?");
        assert!(missing.is_empty(), "no Italian translation for:\n{}", missing.join("\n"));
    }
}
