//! The language of the interface: English (the default) or Russian.
//!
//! The English text is its own key: `tr("Copy")` is "Copy" in English and "Копировать" in Russian, and a
//! text missing from the Russian table stays English (and is noted: a check runs the screens in Russian and
//! asks for none). Texts with values in them: `t!("Delete {}?", what)` — `{}` in order, or `{0}`, `{1}` where a
//! translation needs another order.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Ru];

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    pub fn parse(s: &str) -> Option<Lang> {
        Lang::ALL.into_iter().find(|l| l.code() == s)
    }

    /// Its name in itself, as language menus show it.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }
}

// The language is the program's, for every thread (workers write texts too). Tests run side by side in one
// process: there each thread has its own, so a check in Russian doesn't change the others.
#[cfg(not(test))]
static LANG: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static LANG: std::cell::Cell<Lang> = const { std::cell::Cell::new(Lang::En) };
}

#[cfg(not(test))]
pub fn lang() -> Lang {
    if LANG.load(std::sync::atomic::Ordering::Relaxed) == 1 { Lang::Ru } else { Lang::En }
}

#[cfg(test)]
pub fn lang() -> Lang {
    LANG.with(|l| l.get())
}

#[cfg(not(test))]
pub fn set(l: Lang) {
    LANG.store(if l == Lang::Ru { 1 } else { 0 }, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub fn set(l: Lang) {
    LANG.with(|c| c.set(l));
}

fn ru() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| RU.iter().copied().collect())
}

/// Texts asked for in Russian that the table doesn't have.
static MISSING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// `en` in the current language.
pub fn tr(en: &str) -> &str {
    // Nothing to translate in `/`, `~`, `*.*`.
    if lang() == Lang::En || !en.bytes().any(|b| b.is_ascii_alphabetic()) {
        return en;
    }
    match ru().get(en) {
        Some(t) => t,
        None => {
            MISSING.lock().unwrap().insert(en.to_string());
            en
        }
    }
}

/// `tr` for names that may or may not be texts of the program (a root's title: "All buckets" of S3, or the
/// name of a disk): translated if known, never noted as missing.
pub fn tr_quiet(en: &str) -> &str {
    if lang() == Lang::En {
        return en;
    }
    ru().get(en).copied().unwrap_or(en)
}

/// An error for a person, in the current language: `/a/b: not found` → `/a/b: не найдено`. The sources speak
/// English (their messages are made in workers and in other crates); the parts they are made of are
/// translated here, a path or a server's own words stay as they are.
pub fn error(msg: &str) -> String {
    if lang() == Lang::En {
        return msg.to_string();
    }
    let mut s = msg.to_string();
    for (en, ru) in ERRORS {
        if s.contains(en) {
            s = s.replace(en, ru);
        }
    }
    s
}

/// `error` of a source's error.
pub fn err(e: &vfs::Error) -> String {
    match e {
        vfs::Error::Cancelled => tr("cancelled").to_string(),
        _ => error(&e.to_string()),
    }
}

/// The texts that were asked for in Russian and stayed English (for the checks).
#[cfg(test)]
pub fn missing() -> Vec<String> {
    MISSING.lock().unwrap().iter().cloned().collect()
}

/// `template` with `{}` (in order) or `{0}`, `{1}` (by number) replaced by `args`.
pub fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('}').filter(|&j| after[..j].bytes().all(|b| b.is_ascii_digit())) else {
            out.push('{');
            rest = after;
            continue;
        };
        let n = if j == 0 {
            next += 1;
            next - 1
        } else {
            after[..j].parse().unwrap_or(usize::MAX)
        };
        if let Some(a) = args.get(n) {
            out.push_str(&a.to_string());
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    out
}

/// `t!("Copy")` — the text in the current language; `t!("Delete {}?", what)` — with the values put in.
#[macro_export]
macro_rules! t {
    ($en:expr) => {
        $crate::i18n::tr($en)
    };
    ($en:expr, $($a:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($en), &[$(&$a as &dyn std::fmt::Display),+])
    };
}

/// `n` and a noun in its form for `n`: `1 file`, `3 files`; `1 файл`, `3 файла`, `5 файлов`.
pub fn count(n: u64, one: &str) -> String {
    let num = crate::format::bytes(n);
    if lang() == Lang::En {
        return if n == 1 { format!("1 {one}") } else { format!("{num} {one}s") };
    }
    let Some(&(_, forms)) = NOUNS.iter().find(|(en, _)| *en == one) else {
        MISSING.lock().unwrap().insert(format!("noun: {one}"));
        return format!("{num} {one}");
    };
    let (d, h) = (n % 10, n % 100);
    let form = if d == 1 && h != 11 {
        forms[0]
    } else if (2..=4).contains(&d) && !(12..=14).contains(&h) {
        forms[1]
    } else {
        forms[2]
    };
    format!("{num} {form}")
}

/// `3 of 10 files`; `3 из 10 файлов` (after «из» the noun is in the genitive).
pub fn of_count(done: u64, total: u64, one: &str) -> String {
    if lang() == Lang::En {
        return format!("{} of {}", crate::format::bytes(done), count(total, one));
    }
    let Some(&(_, forms)) = GENITIVE.iter().find(|(en, _)| *en == one) else {
        MISSING.lock().unwrap().insert(format!("noun: {one}"));
        return format!("{done} из {total} {one}");
    };
    let form = if total % 10 == 1 && total % 100 != 11 { forms[0] } else { forms[1] };
    format!("{} из {} {form}", crate::format::bytes(done), crate::format::bytes(total))
}

/// A number with one decimal: `5.3`; `5,3` in Russian.
pub fn decimal(x: f64) -> String {
    let s = format!("{x:.1}");
    if lang() == Lang::Ru { s.replace('.', ",") } else { s }
}

/// Russian nouns: for 1 (21, 31…), for 2–4 (22–24…), for the rest.
const NOUNS: &[(&str, [&str; 3])] = &[
    ("file", ["файл", "файла", "файлов"]),
    ("folder", ["папка", "папки", "папок"]),
    ("object", ["объект", "объекта", "объектов"]),
    ("name", ["имя", "имени", "имён"]),
    ("path", ["путь", "пути", "путей"]),
    ("line", ["строка", "строки", "строк"]),
    ("error", ["ошибка", "ошибки", "ошибок"]),
    ("byte", ["байт", "байта", "байт"]),
];

/// Parts of error messages (vfs, ops, the network sources), English → Russian; the longer first where one
/// holds another.
const ERRORS: &[(&str, &str)] = &[
    // vfs::Error
    (": not found", ": не найдено"),
    (": already exists", ": уже существует"),
    (": permission denied", ": нет доступа"),
    (": not a folder", ": это не папка"),
    (": is a folder", ": это папка"),
    (": folder is not empty", ": папка не пуста"),
    ("not supported by this source", "этот источник так не умеет"),
    // ops
    (" is read-only", " — только для чтения"),
    ("cannot copy a folder into itself", "нельзя скопировать папку в саму себя"),
    ("cannot move a folder into itself", "нельзя перенести папку в саму себя"),
    (" is the same object", " — тот же объект"),
    (" is the same file", " — тот же файл"),
    (": link to a folder, skipped", ": ссылка на папку, пропущена"),
    ("not a regular file (a pipe, socket or device), skipped", "не обычный файл (канал, сокет или устройство), пропущен"),
    ("not a regular file (a pipe, socket or device)", "не обычный файл (канал, сокет или устройство)"),
    (": the link stays, not everything behind it was copied", ": ссылка осталась, скопировано не всё, на что она указывает"),
    ("cannot copy a file onto itself", "нельзя скопировать файл в самого себя"),
    (": a folder with this name exists", ": есть папка с таким именем"),
    ("bad folder name", "недопустимое имя папки"),
    ("bad file name", "недопустимое имя файла"),
    // SFTP
    (": the server did not accept the password.", ": сервер не принял пароль."),
    (": the server did not accept the key ", ": сервер не принял ключ "),
    (
        ": no key to sign in with (no ssh-agent, no ~/.ssh/id_*): type a password or a key file.",
        ": нечем войти (нет ssh-agent и ~/.ssh/id_*): введите пароль или файл ключа.",
    ),
    (": the server accepted none of: ", ": сервер не принял ни один из: "),
    (". Type a password or a key file.", ". Введите пароль или файл ключа."),
    (" key(s) of ssh-agent", " ключ(ей) ssh-agent"),
    ("no answer from the server", "сервер не отвечает"),
    (": no answer in ", ": нет ответа за "),
    (": no connection in ", ": нет соединения за "),
    (": no SFTP on this server", ": на этом сервере нет SFTP"),
    (" is not a port", " — не номер порта"),
    (
        "The server offers a certificate; only plain host keys are supported.",
        "Сервер предлагает сертификат; поддерживаются только обычные ключи хоста.",
    ),
    ("Connection refused", "В соединении отказано"),
    ("connection refused", "в соединении отказано"),
    // S3
    ("files live inside a bucket", "файлы хранятся только внутри бакета"),
    // WebDAV
    ("the server did not accept the user name or password", "сервер не принял имя пользователя или пароль"),
    ("locked by another program", "заблокировано другой программой"),
    ("no space left on the server", "на сервере нет места"),
    (": not a WebDAV folder", ": это не папка WebDAV"),
    (": only http:// and https:// addresses", ": только адреса http:// и https://"),
    (" is not an address: type http://… or https://…", " — не адрес: введите http://… или https://…"),
    (": no server in the address", ": в адресе нет сервера"),
    ("no data from the storage for ", "нет данных от хранилища уже "),
    // FTP
    ("): choose FTP without encryption to connect anyway", "): выберите FTP без шифрования, чтобы всё равно подключиться"),
    ("the server does not offer TLS (", "сервер не предлагает TLS ("),
    ("): choose FTPS", "): выберите FTPS"),
    ("the server wants encryption (", "сервер требует шифрования ("),
    ("no address for this name", "у этого имени нет адреса"),
    ("the server closed the connection", "сервер закрыл соединение"),
    ("no answer to the TLS handshake", "нет ответа на рукопожатие TLS"),
    ("the listing stopped coming", "список перестал приходить"),
    ("the server takes no data", "сервер не принимает данные"),
    ("a line break in a name can't be sent over FTP", "имя с переводом строки нельзя передать по FTP"),
    ("the connection is closed", "соединение закрыто"),
    ("the server refused: ", "сервер отказал: "),
    ("not an FTP reply: ", "это не ответ FTP: "),
    ("sign-in: ", "вход: "),
    ("the upload was finished", "загрузка уже завершена"),
    (": no data for ", ": нет данных уже "),
    (
        "it is not signed by an authority this Mac trusts (a self-signed certificate?)",
        "он не подписан центром, которому доверяет этот Mac (самоподписанный сертификат?)",
    ),
    ("it is issued for another name", "он выдан на другое имя"),
    ("it has expired", "срок его действия истёк"),
    ("it is not valid yet", "он ещё не действует"),
    ("it has been revoked", "он отозван"),
    ("it is self-signed", "он самоподписанный"),
    ("certificate not trusted, fingerprint ", "сертификату нет доверия, отпечаток "),
];

/// Russian nouns after «из»: for 1 (21, 31…), for the rest.
const GENITIVE: &[(&str, [&str; 2])] = &[("file", ["файла", "файлов"]), ("object", ["объекта", "объектов"])];

/// English → Russian.
const RU: &[(&str, &str)] = &[
    // Units
    ("B", "Б"),
    ("KB", "КБ"),
    ("MB", "МБ"),
    ("GB", "ГБ"),
    ("{} s", "{} с"),
    ("{} min {} s", "{} мин {} с"),
    ("{} h {} min", "{} ч {} мин"),
    // The toolbar, the settings, Help
    ("Back to the previous folder ({})", "Назад, в предыдущую папку ({})"),
    ("Forward ({})", "Вперёд ({})"),
    ("Refresh", "Обновить"),
    ("Read the active panel again (Ctrl+R)", "Перечитать активную панель (Ctrl+R)"),
    ("Connect…", "Подключиться…"),
    ("A new connection, or the remembered ones (Ctrl+F)", "Новое подключение или сохранённые (Ctrl+F)"),
    ("Stop waiting for this connection", "Не ждать этого подключения"),
    ("Settings: theme, language", "Настройки: тема, язык"),
    ("Theme", "Тема"),
    ("Light", "Светлая"),
    ("Dark", "Тёмная"),
    ("As the system", "Как в системе"),
    ("Language", "Язык"),
    ("Help", "Справка"),
    ("Keys (F1), about the program", "Клавиши (F1), о программе"),
    ("Keys", "Клавиши"),
    ("About uison-commander", "О программе uison-commander"),
    ("Version {}", "Версия {}"),
    (
        "A two-panel file manager in the spirit of Total Commander: the disk, S3 and SFTP servers side by side, with the keys of TC.",
        "Двухпанельный файловый менеджер в духе Total Commander: диск, S3 и серверы SFTP бок о бок, с клавишами TC.",
    ),
    ("⌘ works as ⌃ (Ctrl) everywhere, except ⌘Q and ⌘⌫.", "⌘ везде работает как ⌃ (Ctrl), кроме ⌘Q и ⌘⌫."),
    // The panels
    ("Alt+F1 / Alt+F2: source and drive", "Alt+F1 / Alt+F2: источник и диск"),
    ("Name", "Имя"),
    ("Ext", "Тип"),
    ("Size", "Размер"),
    ("Date", "Дата"),
    ("Attr", "Атр."),
    ("Loading…", "Загрузка…"),
    ("Folder is empty", "Папка пуста"),
    ("Quick search:", "Быстрый поиск:"),
    ("Retry", "Повторить"),
    ("Can't open {}: {}", "Не удалось открыть {}: {}"),
    ("Empty", "Пусто"),
    ("Selected {} ({}) of {} ({})", "Выделено {} ({}), всего {} ({})"),
    ("{} free of {}", "свободно {} из {}"),
    ("This Mac", "Этот Mac"),
    ("This computer", "Этот компьютер"),
    // The F-keys and the context menu
    ("View", "Просмотр"),
    ("Edit", "Правка"),
    ("Copy", "Копировать"),
    ("Move", "Переместить"),
    ("New folder", "Новая папка"),
    ("Delete", "Удалить"),
    ("Quit", "Выход"),
    ("Open", "Открыть"),
    ("Rename", "Переименовать"),
    ("Copy path", "Копировать путь"),
    ("Properties", "Свойства"),
    // The keys (F1)
    ("Moving around", "Перемещение"),
    ("Selecting", "Выделение"),
    ("Files", "Файлы"),
    ("Panels", "Панели"),
    ("The program", "Программа"),
    ("Cursor up / down", "Курсор вверх / вниз"),
    ("A page up / down", "На страницу вверх / вниз"),
    ("First / last row", "Первая / последняя строка"),
    ("Enter a folder; open a file (the system's app, or the viewer)", "Войти в папку; открыть файл (программой системы или в просмотре)"),
    ("Up a level; the cursor lands on the folder left", "На уровень выше; курсор встаёт на папку, из которой вышли"),
    ("To the root", "В корень"),
    ("Back to the previous folder", "Назад, в предыдущую папку"),
    ("Forward again", "Снова вперёд"),
    ("Quick search by the start of a name; Esc closes it", "Быстрый поиск по началу имени; Esc закрывает его"),
    ("Select / unselect, then down", "Выделить / снять выделение и вниз"),
    ("Select / unselect, then down; on a folder also counts its size", "Выделить / снять выделение и вниз; у папки ещё и считает размер"),
    ("Select while moving up", "Выделять, двигаясь вверх"),
    ("Select while moving down", "Выделять, двигаясь вниз"),
    ("Select by a mask (*.txt)", "Выделить по маске (*.txt)"),
    ("Unselect by a mask", "Снять выделение по маске"),
    ("Invert the selection (files)", "Обратить выделение (файлы)"),
    ("Select all", "Выделить всё"),
    ("Unselect all", "Снять всё выделение"),
    ("View (text, hex, pictures)", "Просмотр (текст, hex, картинки)"),
    ("Edit in the system's text editor", "Правка в текстовом редакторе системы"),
    ("New file, then edit it", "Новый файл и его правка"),
    ("Copy to the other panel", "Копировать в другую панель"),
    ("Move to the other panel", "Переместить в другую панель"),
    ("Copy here, under another name", "Копировать сюда под другим именем"),
    ("Rename (a mask for several)", "Переименовать (для нескольких — маска)"),
    ("New folder (a/b/c makes all levels)", "Новая папка (a/b/c создаёт все уровни)"),
    ("Properties: name, permissions, the source's actions", "Свойства: имя, права, действия источника"),
    ("The context menu", "Контекстное меню"),
    ("Copy the full paths (⌥⌘C as in Finder)", "Копировать полные пути (⌥⌘C, как в Finder)"),
    ("Copy the names", "Копировать имена"),
    ("The other panel", "Другая панель"),
    ("Source and folder of the left panel", "Источник и папка левой панели"),
    ("Source and folder of the right panel", "Источник и папка правой панели"),
    ("Connect: a new connection, or the remembered ones", "Подключиться: новое подключение или сохранённые"),
    ("Sort by name (again: reverse)", "Сортировать по имени (ещё раз — обратный порядок)"),
    ("Sort by extension", "Сортировать по расширению"),
    ("Sort by date", "Сортировать по дате"),
    ("Sort by size", "Сортировать по размеру"),
    ("Read the panel again", "Перечитать панель"),
    ("Swap the panels", "Поменять панели местами"),
    ("The folder under the cursor in the other panel", "Папку под курсором — в другую панель"),
    ("Show / hide hidden files", "Показать / скрыть скрытые файлы"),
    ("This help", "Эта справка"),
    ("Letters", "Буквы"),
    // Dialogs: common
    ("OK", "ОК"),
    ("Cancel", "Отмена"),
    ("Close", "Закрыть"),
    ("Create", "Создать"),
    ("Choose…", "Выбрать…"),
    ("Busy", "Занято"),
    ("Read-only", "Только чтение"),
    ("{} is read-only.", "{} — только для чтения."),
    ("An operation is running. Cancel it and quit?", "Идёт операция. Отменить её и выйти?"),
    ("Another operation is running. Wait for it or cancel it.", "Уже идёт другая операция. Дождитесь её или отмените."),
    ("Select", "Выделить"),
    ("Unselect", "Снять выделение"),
    ("Mask (several with ';'):", "Маска (несколько — через ';'):"),
    ("Name (a/b/c makes all levels):", "Имя (a/b/c создаёт все уровни):"),
    ("New file", "Новый файл"),
    ("Name:", "Имя:"),
    ("New name:", "Новое имя:"),
    ("Copied: {}", "Скопировано: {}"),
    ("Copied to the clipboard", "Скопировано в буфер обмена"),
    ("Copied to the clipboard: {}", "Скопировано в буфер обмена: {}"),
    ("Cannot create the folder", "Не удалось создать папку"),
    ("Cannot create the file", "Не удалось создать файл"),
    ("Action failed", "Действие не выполнено"),
    ("cancelled", "отменено"),
    // Copy, move, delete
    ("the folder \"{}\"", "папку \"{}\""),
    ("{} {} to:\nA new name, or a mask such as *.bak", "{} {} в:\nНовое имя или маска, например *.bak"),
    ("{} {} to {}:\nA folder, a new name, or a mask such as *.bak", "{} {} в {}:\nПапка, новое имя или маска, например *.bak"),
    ("Copying {}", "Копирование: {}"),
    ("Moving {}", "Перемещение: {}"),
    ("Deleting {}", "Удаление: {}"),
    ("… and {} more", "… и ещё {}"),
    ("Delete {}?\n{}\n\nThere is no trash yet: this cannot be undone.", "Удалить {}?\n{}\n\nКорзины пока нет: это нельзя отменить."),
    ("Counting… {}", "Подсчёт… {}"),
    ("{} of {}", "{} из {}"),
    ("{}/s", "{}/с"),
    ("{} left", "осталось {}"),
    ("File", "Файл"),
    ("Total", "Всего"),
    ("Show", "Показать"),
    ("Background", "В фоне"),
    ("{}: {} done, {} skipped, {} failed", "{}: готово {}, пропущено {}, с ошибкой {}"),
    ("{}: cancelled ({} done)", "{}: отменено (готово {})"),
    ("{}: done", "{}: готово"),
    (" (newer)", " (новее)"),
    (" (larger)", " (больше)"),
    ("{}\n\nCopying:     {}\nAlready there:  {}", "{}\n\nКопируется:  {}\nУже есть:     {}"),
    ("The file already exists", "Файл уже существует"),
    ("Overwrite", "Заменить"),
    ("Overwrite all", "Заменить все"),
    ("Overwrite older", "Заменить старые"),
    ("Skip", "Пропустить"),
    ("Skip all", "Пропустить все"),
    ("Rename…", "Переименовать…"),
    ("Something failed", "Ошибка"),
    // Properties
    ("Path", "Путь"),
    ("Source", "Источник"),
    ("Kind", "Вид"),
    ("Modified", "Изменён"),
    ("Permissions", "Права"),
    ("Read", "Чтение"),
    ("Write", "Запись"),
    ("Execute", "Запуск"),
    ("The permissions as a number (chmod)", "Права числом (chmod)"),
    ("Owner", "Владелец"),
    ("Group", "Группа"),
    ("Others", "Остальные"),
    ("Folder", "Папка"),
    ("{} file", "Файл {}"),
    ("Link to a folder → {}", "Ссылка на папку → {}"),
    ("Link to a file → {}", "Ссылка на файл → {}"),
    ("Not a regular file (a pipe, socket or device)", "Не обычный файл (канал, сокет или устройство)"),
    ("Cannot change the properties", "Не удалось изменить свойства"),
    ("Cannot rename", "Не удалось переименовать"),
    ("\"{}\" is not a valid name", "\"{}\" — недопустимое имя"),
    ("Storage class", "Класс хранения"),
    ("ETag", "ETag"),
    // Sources and drives (Alt+F1), connections
    ("Source and folder for the left panel", "Источник и папка для левой панели"),
    ("Source and folder for the right panel", "Источник и папка для правой панели"),
    ("reading…", "читается…"),
    ("All buckets", "Все бакеты"),
    ("Saved connections", "Сохранённые подключения"),
    ("Enter connects · Delete forgets", "Enter — подключиться · Delete — забыть"),
    ("asks the password · Delete forgets", "спросит пароль · Delete — забыть"),
    ("Connections", "Подключения"),
    ("Connect", "Подключиться"),
    ("Test", "Проверить"),
    ("Edit…", "Изменить…"),
    ("connected", "подключено"),
    ("asks the password", "спросит пароль"),
    (
        "Enter or a double click connects. Passwords are kept in the Keychain.",
        "Enter или двойной щелчок — подключиться. Пароли хранятся в Связке ключей.",
    ),
    (
        "Nothing here yet: add a connection — it is kept here once it works.",
        "Пока пусто: добавьте подключение — оно сохранится здесь, когда заработает.",
    ),
    ("Add", "Добавить"),
    ("New {} connection", "Новое подключение: {}"),
    ("{} connection", "Подключение: {}"),
    ("{} — change and connect.", "{} — измените и подключитесь."),
    ("{} — the password or key is not remembered.", "{} — пароль или ключ не сохранён."),
    ("Connecting", "Подключение"),
    ("Another connection is being made.", "Уже идёт другое подключение."),
    ("Connecting to {}…", "Подключение к {}…"),
    ("Connecting…", "Подключение…"),
    ("Checking the connection…", "Проверка подключения…"),
    ("It works: connected to {}.", "Работает: подключено к {}."),
    ("Connected to {}", "Подключено: {}"),
    ("Cannot connect", "Не удалось подключиться"),
    ("Remember the password in the Keychain", "Запомнить пароль в Связке ключей"),
    ("empty — the one in the Keychain", "пусто — тот, что в Связке ключей"),
    ("A server never seen before", "Незнакомый сервер"),
    (
        "This is the first connection to {}. Its key fingerprint is\n\n{}\n\nIf you expected this server (its administrator can confirm the fingerprint), trust it: it will be checked on every connection from now on.",
        "Это первое подключение к {}. Отпечаток его ключа:\n\n{}\n\nЕсли вы ждали этот сервер (отпечаток может подтвердить его администратор), доверьтесь ему: отпечаток будет проверяться при каждом подключении.",
    ),
    ("Trust and connect", "Доверять и подключиться"),
    // The connectors
    (
        "An AWS account: its access key and the secret of it. Each bucket's region is found by itself.",
        "Учётная запись AWS: ключ доступа и его секрет. Регион каждого бакета определяется сам.",
    ),
    ("S3-compatible storage", "S3-совместимое хранилище"),
    (
        "A storage that speaks the S3 protocol: MinIO, Cloudflare R2, Wasabi, Backblaze B2, Yandex Object Storage.",
        "Хранилище с протоколом S3: MinIO, Cloudflare R2, Wasabi, Backblaze B2, Yandex Object Storage.",
    ),
    ("Display name", "Отображаемое имя"),
    ("Endpoint", "Адрес (endpoint)"),
    ("Region", "Регион"),
    ("Access key ID", "Ключ доступа"),
    ("Secret access key", "Секретный ключ"),
    ("e.g. Work AWS; empty — s3:aws", "например, Рабочий AWS; пусто — s3:aws"),
    ("e.g. MinIO at home; empty — s3:<host>", "например, Домашний MinIO; пусто — s3:<адрес>"),
    (
        "required: http://localhost:9000, https://<account>.r2.cloudflarestorage.com",
        "обязательно: http://localhost:9000, https://<account>.r2.cloudflarestorage.com",
    ),
    ("empty — us-east-1", "пусто — us-east-1"),
    (
        "empty — ~/.aws/credentials (AWS_PROFILE, or default), or the AWS_* variables",
        "пусто — ~/.aws/credentials (AWS_PROFILE или default) или переменные AWS_*",
    ),
    ("no endpoint: type the storage's address, e.g. http://localhost:9000", "нет адреса: введите адрес хранилища, например http://localhost:9000"),
    (
        "no keys: type them, or name a profile of ~/.aws/credentials (\"{}\" has none)",
        "нет ключей: введите их или укажите профиль из ~/.aws/credentials (в \"{}\" их нет)",
    ),
    ("Files on a server over SSH.", "Файлы на сервере через SSH."),
    ("Files on a WebDAV server: Nextcloud, ownCloud, a NAS, Apache.", "Файлы на сервере WebDAV: Nextcloud, ownCloud, NAS, Apache."),
    (
        "Files on an FTP server: a hosting, a NAS, a router. FTPS encrypts the password and the files.",
        "Файлы на сервере FTP: хостинг, NAS, роутер. FTPS шифрует пароль и файлы.",
    ),
    ("FTP / FTPS", "FTP / FTPS"),
    ("Encryption", "Шифрование"),
    ("FTPS: TLS after connecting (port 21, usual)", "FTPS: TLS после подключения (порт 21, обычно так)"),
    ("FTPS: TLS from the start (port 990)", "FTPS: TLS с самого начала (порт 990)"),
    ("None: plain FTP, the password goes in the clear", "Нет: обычный FTP, пароль передаётся открыто"),
    ("host or host:port", "host или host:port"),
    ("empty — anonymous", "пусто — anonymous"),
    (
        "First connection to {}: its certificate ({}) is saved and checked next time.",
        "Первое подключение к {}: его сертификат ({}) сохранён и будет проверяться.",
    ),
    (
        "Connected to {}, but its certificate could not be saved ({}): it will be asked again.",
        "Подключено к {}, но сертификат не удалось сохранить ({}): вопрос будет задан снова.",
    ),
    ("A certificate this Mac does not trust", "Сертификат, которому этот Mac не доверяет"),
    (
        "The certificate of {} is not trusted: {}. Its fingerprint is\n\n{}\n\nIf you expected this server (its administrator can confirm the fingerprint), trust it: it will be checked on every connection from now on.",
        "Сертификату {} нет доверия: {}. Его отпечаток:\n\n{}\n\nЕсли вы ждали этот сервер (отпечаток может подтвердить его администратор), доверьтесь ему: отпечаток будет проверяться при каждом подключении.",
    ),
    ("The server's certificate has changed", "Сертификат сервера изменился"),
    (
        "The certificate of {} is not the one trusted before. Its fingerprint now is\n\n{}\n\nA new certificate of the server, or someone in between. Trust it only if its administrator confirms the fingerprint.",
        "Сертификат {} — не тот, которому вы доверяли. Его отпечаток теперь:\n\n{}\n\nЭто новый сертификат сервера — или кто-то посередине. Доверяйтесь, только если администратор подтвердит отпечаток.",
    ),
    ("Address", "Адрес"),
    ("User", "Пользователь"),
    ("empty — no sign-in", "пусто — без входа"),
    ("the password, or an app password of Nextcloud", "пароль или пароль приложения Nextcloud"),
    ("https://cloud.example.com/remote.php/dav/files/me/", "https://cloud.example.com/remote.php/dav/files/me/"),
    ("Server", "Сервер"),
    ("Password", "Пароль"),
    ("Key file", "Файл ключа"),
    ("Key passphrase", "Пароль ключа"),
    ("user@host:port, or a Host of ~/.ssh/config", "user@host:port или Host из ~/.ssh/config"),
    ("empty — sign in with a key", "пусто — вход по ключу"),
    ("empty — ssh-agent, then ~/.ssh/id_ed25519, id_ecdsa, id_rsa", "пусто — ssh-agent, затем ~/.ssh/id_ed25519, id_ecdsa, id_rsa"),
    ("if the key has one", "если он у ключа есть"),
    ("empty — the home folder; /var/www/site, or www from the home", "пусто — домашняя папка; /var/www/site или www от домашней"),
    ("no user: type user@host", "нет пользователя: введите user@host"),
    ("Choose the private key (not the .pub file)", "Выберите закрытый ключ (не файл .pub)"),
    // The sources' actions
    ("Copy link", "Копировать ссылку"),
    ("Link type", "Вид ссылки"),
    ("Link", "Ссылка"),
    ("Public (the object must be public)", "Публичная (объект должен быть публичным)"),
    ("Signed, for 1 hour", "Подписанная, на 1 час"),
    ("Signed, for 1 day", "Подписанная, на 1 день"),
    ("Signed, for 7 days", "Подписанная, на 7 дней"),
    ("Visibility", "Видимость"),
    ("Who can read", "Кто может читать"),
    ("Only the owner (private)", "Только владелец (private)"),
    ("Everyone (public-read)", "Все (public-read)"),
    // The viewer
    ("Text", "Текст"),
    ("Hex", "Hex"),
    ("Wrap", "Перенос"),
    ("text", "текст"),
    ("text, wrap", "текст, перенос"),
    ("hex", "hex"),
    ("picture", "картинка"),
    ("Esc close", "Esc — закрыть"),
    (
        "Esc close  ·  1 text  ·  3 hex  ·  W wrap  ·  F7 search  ·  F3 / Shift+F3 next / previous",
        "Esc — закрыть  ·  1 — текст  ·  3 — hex  ·  W — перенос  ·  F7 — поиск  ·  F3 / Shift+F3 — следующее / предыдущее",
    ),
    ("Search", "Поиск"),
    ("Text (any case):", "Текст (регистр не важен):"),
    ("Find", "Найти"),
    ("searching…", "поиск…"),
    ("found at {}", "найдено, смещение {}"),
    ("\"{}\" not found", "\"{}\" не найдено"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_in_order_and_by_number() {
        assert_eq!(fill("Copy {} to {}", &[&"a", &3]), "Copy a to 3");
        assert_eq!(fill("{1} ← {0}", &[&"a", &"b"]), "b ← a");
        assert_eq!(fill("{x} and {", &[&1]), "{x} and {");
    }

    #[test]
    fn russian_plurals() {
        set(Lang::Ru);
        let c = |n| count(n, "file");
        assert_eq!(
            [c(1), c(2), c(5), c(11), c(21), c(22), c(112), c(1001)],
            ["1 файл", "2 файла", "5 файлов", "11 файлов", "21 файл", "22 файла", "112 файлов", "1 001 файл"]
        );
        assert_eq!(decimal(5.25), "5,2");
        assert_eq!([of_count(0, 1, "file"), of_count(2, 21, "file"), of_count(3, 5, "object")], ["0 из 1 файла", "2 из 21 файла", "3 из 5 объектов"]);
        set(Lang::En);
        assert_eq!((count(1, "file"), count(1200, "folder")), ("1 file".to_string(), "1 200 folders".to_string()));
    }

    /// Every translation has the values of its English text, and no text is twice in the table.
    #[test]
    fn the_table_is_sound() {
        let holes = |s: &str| {
            let mut n = 0;
            let mut rest = s;
            while let Some(i) = rest.find('{') {
                let after = &rest[i + 1..];
                if let Some(j) = after.find('}').filter(|&j| after[..j].bytes().all(|b| b.is_ascii_digit())) {
                    n += 1;
                    rest = &after[j + 1..];
                } else {
                    rest = after;
                }
            }
            n
        };
        let mut seen = BTreeSet::new();
        for (en, ru) in RU {
            assert!(seen.insert(en), "twice in the table: {en}");
            assert_eq!(holes(en), holes(ru), "values differ: {en} → {ru}");
            assert!(!ru.is_empty(), "{en}");
        }
    }
}
