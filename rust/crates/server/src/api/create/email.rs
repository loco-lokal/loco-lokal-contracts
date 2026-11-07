//! Email address handling of submission creation: `EmailTypo` (the `email_typo` gem),
//! `Submissions.normalize_email` and `Submissions.parse_emails`.

use std::sync::LazyLock;

use fancy_regex::Regex;

/// One `gsub` step.
struct Fix(Regex, &'static str);

fn fix(pattern: &str, replacement: &'static str) -> Fix {
    Fix(Regex::new(pattern).expect("valid regex"), replacement)
}

fn apply(fixes: &[Fix], email: &str) -> String {
    fixes.iter().fold(email.to_owned(), |acc, Fix(re, to)| {
        re.replace_all(&acc, *to).into_owned()
    })
}

/// `EmailTypo::DotCom`.
static DOT_COM: LazyLock<Vec<Fix>> = LazyLock::new(|| {
    vec![
        fix(r"\.co[mn]\.com", ".com"),
        fix(r"\.com\.$", ".com"),
        fix(r"\.com(?!cast|\.|@).{1,3}$", ".com"),
        fix(r"\.co[^op]$", ".com"),
        fix(r"\.c*(c|ck|ci|coi|l|m|n|o|op|cp|0|9)*m+o*$", ".com"),
        fix(r"\.(c|f|v|x)o+(m|n)$", ".com"),
    ]
});

/// `EmailTypo.default_processors`, in order.
static PROCESSORS: LazyLock<Vec<Fix>> = LazyLock::new(|| {
    let w = "[0-9A-Za-z_]";
    let mut fixes = vec![
        // RemoveMailTo
        fix(r"\Amailto:", ""),
        // RemoveInvalidChars
        fix(r#"(\s|[#`'"\\|])*"#, ""),
        fix(r"/", "."),
        fix(r"(,|\.\.)", "."),
        fix("!", "1"),
        fix("@@", "@"),
        // TransposedPeriods
        fix(r"c\.om$", ".com"),
        fix(r"n\.et$", ".net"),
        // PeriodAroundAtSign
        fix(r"(\.@|@\.)", "@"),
        // DifferentTlds
        fix(r"\.(o\.uk|co\.k|couk|co\.u[kmnlj]{0,2})$", ".co.uk"),
        fix(r"\.(cojp|co\.lp|co\.p)$", ".co.jp"),
        fix(r"\.(com?br|com?\.[bv]r+)$", ".com.br"),
        fix(r"\.(r+(u+(?!n).|y)|r)$", ".ru"),
        fix(r"\.i+t+$", ".it"),
        fix(r"\.f+[re]+$", ".fr"),
        fix(r"\.de+(?!v).$", ".de"),
        fix(r"\.jn$", ".in"),
        fix(r"\.lde$", ".de"),
        fix(r"\.oprg$", ".org"),
        fix(r"\.gob(\b|\.)", ".gov"),
        fix(r"\.edi?(\b|\.)", ".edu"),
        fix(r"\.mx.{1,2}$", ".mx"),
        fix(r"\.[com.]{2,3}ar$", ".com.ar"),
        fix(r"\.[com.]{2,3}au$", ".com.au"),
        // FixExtraneousLetterDotCom
        fix(r"\..com$", ".com"),
        // FixExtraneousNumbers
        fix(r"\.?[0-9]+$", ""),
        // AddMissingM
        fix(r"(aol|googlemail|gmail|hotmail|yahoo).co$", "$1.com"),
        // AddMissingPeriod
        fix(r"([^.])(com|org|net)$", "$1.$2"),
        // Providers
        fix(r"@co?(m|n)a?cas?t{0,2}\.", "@comcast."),
        fix(r"@sbc?gl?ob[al]{0,2}l?\.", "@sbcglobal."),
        fix(r"@ver?i?z?on\.", "@verizon."),
        fix(r"@icl{0,2}oud\.", "@icloud."),
        fix(r"@outl?ook?\.", "@outlook."),
        // Aol
        fix(r"@(ol|ao|ail)\.", "@aol."),
    ];
    fixes.extend(DOT_COM.iter().map(|Fix(re, to)| Fix(re.clone(), to)));
    fixes.extend([
        // DotNet
        fix(r"\.(nte*|n*et*|ney)$", ".net"),
        fix(r"\.met$", ".net"),
        // DotOrg
        fix(r"\.og?r?g{0,2}$", ".org"),
        // Gmail
        fix(
            r"@ga?e?i?o?r?g?[nm]{0,2}s?[ail]{1,2}[aiklmou]{0,3}\.(?!gov|edu|ac\.in)",
            "@gmail.",
        ),
        fix(&format!(r"@gmail\.(com\.{w}+|c)$"), "@gmail.com"),
        // Googlemail
        fix(
            r"@go{0,3}g{0,2}o?le?[mn]?[ail]{1,2}m?[aikl]{0,3}\.(?!gov|edu|ac\.in)",
            "@googlemail.",
        ),
        // Hotmail
        fix(
            r"@h((?!anmail)(i|o|p)?y?t?o?a?r?m?n?t?m?[aikl]{1,3}l?)\.",
            "@hotmail.",
        ),
        fix(r"@otmail\.com", "@hotmail.com"),
        fix(r"@hotmail.com[a-z]+", "@hotmail.com"),
        // Icloud
        fix(r"@icl{0,2}(?:uo|u|o)?d\.", "@icloud."),
        // KnownDotCom
        fix(
            r"@(aol|googlemail|gmail|hotmail|yahoo|icloud|outlook)\.(co|net|org)$",
            "@$1.com",
        ),
        // Yahoo
        fix(r"@(ya|yh|ua|ah)+h*a*o+\.", "@yahoo."),
    ]);
    fixes
});

/// `EmailTypo::DotCom.call`.
#[must_use]
pub fn dot_com(email: &str) -> String {
    apply(&DOT_COM, email)
}

/// `EmailTypo.call`.
#[must_use]
pub fn email_typo(email: &str) -> String {
    apply(&PROCESSORS, &email.to_lowercase())
}

/// `User::FULL_EMAIL_REGEXP`.
static FULL_EMAIL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\A[a-z0-9][.']?(?:(?:[a-z0-9_-]+[.+'])*[a-z0-9_-]+)*@(?:[a-z0-9]+[.-])*[a-z0-9]+\.[a-z]{2,}\z",
    )
    .expect("valid regex")
});

/// `User::EMAIL_REGEXP`.
static EMAIL: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[^@;,<>\s]+@[^@;,<>\s]+").expect("valid regex"));

static EMAIL_LIST_SEPARATOR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\s*[;,/]\s*").expect("valid regex"));

/// The check of `BaseValidator#email_format`.
#[must_use]
pub fn valid_email_list(value: &str) -> bool {
    if value.contains('<') {
        return true;
    }
    EMAIL_LIST_SEPARATOR
        .split(value.trim())
        .filter(|e| !e.trim().is_empty())
        .all(|email| FULL_EMAIL.is_match(&dot_com(email)) || email.contains("--"))
}

/// `Submissions.parse_emails` for a string.
#[must_use]
pub fn parse_emails(emails: &str) -> Vec<String> {
    EMAIL
        .find_iter(emails)
        .map(|m| m.as_str().to_owned())
        .collect()
}

const SKIP_EMAIL_FIX_LABELS: &[&str] = &[
    "ga", "gami", "gasi", "gil", "gma", "gmao", "gmi", "gmsi", "goi", "ol",
];

static SKIP_EMAIL_FIX_TLDS: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\.(?:gob(?:\.[0-9A-Za-z_]+)?|om|mm|cm|et|mo|nz|za|ie|mom|free|comsec|unicom|ed(?:\.[0-9A-Za-z_]+){1,2})\z",
    )
    .expect("valid regex")
});

static GMAIL_SUFFIX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)@gmail?\z").expect("valid regex"));
static GMAIL_OTHER_TLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\Agmail\.(?!com\z)").expect("valid regex"));
static COMCAST_VERIZON: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\A(?:comcast|verizon)\.").expect("valid regex"));

fn skip_email_fix(domain: &str, fixed_domain: &str) -> bool {
    if SKIP_EMAIL_FIX_LABELS.contains(&domain.split('.').next().unwrap_or_default()) {
        return true;
    }
    if GMAIL_OTHER_TLD.is_match(fixed_domain).unwrap_or(false) {
        return true;
    }
    COMCAST_VERIZON.is_match(fixed_domain) && !COMCAST_VERIZON.is_match(domain)
}

/// `Submissions.normalize_email`: fixes likely typos in the domain.
#[must_use]
pub fn normalize_email(email: &str) -> Option<String> {
    if email.trim().is_empty() {
        return None;
    }
    let email = email.replace('/', ",");
    if GMAIL_SUFFIX.is_match(&email) {
        return Some(
            GMAIL_SUFFIX
                .replace(&email.to_lowercase(), "@gmail.com")
                .into_owned(),
        );
    }
    if email.contains(',') || SKIP_EMAIL_FIX_TLDS.is_match(&email) || !email.contains('.') {
        return Some(email.to_lowercase());
    }

    let fixed = email_typo(email.strip_prefix('<').unwrap_or(&email));
    if fixed == email {
        return Some(fixed);
    }
    let domain = email.rsplit('@').next().unwrap_or_default().to_lowercase();
    let fixed_domain = fixed.rsplit('@').next().unwrap_or_default();
    if domain == fixed_domain || skip_email_fix(&domain, fixed_domain) {
        return Some(email.to_lowercase());
    }
    let threshold = if fixed_domain.starts_with("hotmail.") {
        2
    } else {
        3
    };
    if strsim::levenshtein(&domain, fixed_domain) > threshold {
        return Some(email.to_lowercase());
    }
    Some(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_typos_like_email_typo() {
        assert_eq!(
            normalize_email("John@GMAIL.con").as_deref(),
            Some("john@gmail.com")
        );
        assert_eq!(
            normalize_email("a@hotmial.com").as_deref(),
            Some("a@hotmail.com")
        );
        assert_eq!(
            normalize_email("a@example.com").as_deref(),
            Some("a@example.com")
        );
        assert_eq!(
            normalize_email("A@Example.com").as_deref(),
            Some("a@example.com")
        );
        assert_eq!(normalize_email("a@gmai").as_deref(), Some("a@gmail.com"));
        assert_eq!(
            normalize_email("a@b.com, c@d.com").as_deref(),
            Some("a@b.com, c@d.com")
        );
        assert_eq!(normalize_email(" "), None);
    }

    #[test]
    fn validates_email_lists() {
        assert!(valid_email_list("a@example.com"));
        assert!(valid_email_list("a@example.com, b@example.org"));
        assert!(valid_email_list("Name <a@example.com>"));
        assert!(!valid_email_list("not-an-email"));
        assert!(valid_email_list("a@example.co"));
    }

    #[test]
    fn parses_emails() {
        assert_eq!(
            parse_emails("a@x.com, b@y.org;c@z.de"),
            ["a@x.com", "b@y.org", "c@z.de"]
        );
    }
}
