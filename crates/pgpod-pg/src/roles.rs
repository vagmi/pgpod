//! Bootstrap SQL: roles and the application database.
//!
//! Rendered once, immediately after `initdb`, and executed over the unix
//! socket as the superuser. Everything here is idempotent so a bootstrap
//! that dies partway through can be re-run.

use pgpod_core::Secret;

use crate::Error;

/// Role standbys authenticate as. Has `REPLICATION` and `LOGIN` and
/// nothing else — it must not be able to read application data.
pub const REPLICATION_ROLE: &str = "streaming_replica";

/// Role the daemon's probes use. Member of `pg_monitor`, which grants the
/// statistics views without granting data access.
pub const MONITOR_ROLE: &str = "pgpod_monitor";

#[derive(Debug, Clone)]
pub struct BootstrapRoles {
    pub superuser: String,
    pub superuser_password: Secret,
    pub replication_password: Secret,
    pub monitor_password: Secret,
    /// The application database and its owner. `None` creates neither,
    /// which is what a restored or cloned instance wants.
    pub app: Option<AppDatabase>,
}

#[derive(Debug, Clone)]
pub struct AppDatabase {
    pub database: String,
    pub owner: String,
    pub owner_password: Secret,
}

impl BootstrapRoles {
    /// SQL statements to run in order, against the `postgres` database.
    ///
    /// Returned as separate statements rather than one script because
    /// `CREATE DATABASE` cannot run inside a transaction block, and the
    /// caller needs to send them individually to get a useful error.
    pub fn statements(&self) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();

        for name in [&self.superuser, REPLICATION_ROLE, MONITOR_ROLE] {
            validate_identifier(name)?;
        }

        // The superuser already exists from initdb; this re-asserts the
        // password so a rotated secret takes effect on restart.
        out.push(format!(
            "ALTER ROLE {} WITH PASSWORD {};",
            quote_ident(&self.superuser),
            quote_literal(self.superuser_password.expose())
        ));

        out.push(create_role_if_absent(
            REPLICATION_ROLE,
            "LOGIN REPLICATION",
            &self.replication_password,
        ));
        out.push(create_role_if_absent(
            MONITOR_ROLE,
            "LOGIN",
            &self.monitor_password,
        ));
        out.push(format!(
            "GRANT pg_monitor TO {};",
            quote_ident(MONITOR_ROLE)
        ));

        if let Some(app) = &self.app {
            validate_identifier(&app.owner)?;
            validate_identifier(&app.database)?;
            out.push(create_role_if_absent(
                &app.owner,
                "LOGIN",
                &app.owner_password,
            ));
        }

        Ok(out)
    }

    /// The database-existence check, and the `CREATE DATABASE` to run if
    /// it comes back empty.
    ///
    /// Separate from [`Self::statements`] because `CREATE DATABASE` cannot
    /// run inside a transaction block or a `DO` block, and has no
    /// `IF NOT EXISTS`. The tempting workaround is psql's `\gexec` — but
    /// that is a *psql meta-command*, not SQL. Sent through `psql -c` or a
    /// driver it does nothing, so the database would silently never be
    /// created. Handing the caller two plain statements keeps this honest.
    pub fn app_database_sql(&self) -> Result<Option<(String, String)>, Error> {
        let Some(app) = &self.app else {
            return Ok(None);
        };
        validate_identifier(&app.owner)?;
        validate_identifier(&app.database)?;
        Ok(Some((
            format!(
                "SELECT 1 FROM pg_database WHERE datname = {};",
                quote_literal(&app.database)
            ),
            format!(
                "CREATE DATABASE {} OWNER {};",
                quote_ident(&app.database),
                quote_ident(&app.owner)
            ),
        )))
    }
}

fn create_role_if_absent(name: &str, attrs: &str, password: &Secret) -> String {
    // A DO block is the only way to get IF NOT EXISTS semantics for roles.
    // The ALTER outside the guard means a re-run rotates the password
    // rather than silently keeping the old one.
    format!(
        "DO $pgpod$ BEGIN \
         IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = {name_lit}) THEN \
         CREATE ROLE {name_ident} {attrs}; \
         END IF; END $pgpod$; \
         ALTER ROLE {name_ident} WITH {attrs} PASSWORD {pw};",
        name_lit = quote_literal(name),
        name_ident = quote_ident(name),
        attrs = attrs,
        pw = quote_literal(password.expose()),
    )
}

/// Identifiers pgpod generates are already constrained, but a database or
/// owner name comes straight from a manifest. Rejecting anything outside
/// `[a-z_][a-z0-9_]*` means quoting is a second line of defence rather
/// than the only one.
fn validate_identifier(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidIdentifier(name.to_string()))
    }
}

/// Quote a SQL identifier, doubling embedded double quotes.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a SQL string literal, doubling embedded single quotes.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles() -> BootstrapRoles {
        BootstrapRoles {
            superuser: "postgres".into(),
            superuser_password: Secret::new("su-pw"),
            replication_password: Secret::new("repl-pw"),
            monitor_password: Secret::new("mon-pw"),
            app: Some(AppDatabase {
                database: "appdb".into(),
                owner: "app".into(),
                owner_password: Secret::new("app-pw"),
            }),
        }
    }

    #[test]
    fn creates_the_three_roles_pgpod_depends_on() {
        let sql = roles().statements().unwrap().join("\n");
        assert!(sql.contains(REPLICATION_ROLE));
        assert!(sql.contains(MONITOR_ROLE));
        assert!(sql.contains("\"app\""));
    }

    #[test]
    fn the_replication_role_gets_replication_but_not_superuser() {
        let sql = roles().statements().unwrap().join("\n");
        let stmt = sql
            .lines()
            .find(|l| l.contains(REPLICATION_ROLE) && l.contains("CREATE ROLE"))
            .expect("a create statement");
        assert!(stmt.contains("REPLICATION"), "got: {stmt}");
        assert!(!stmt.contains("SUPERUSER"), "must not be superuser: {stmt}");
        assert!(
            !stmt.contains("CREATEDB"),
            "must not create databases: {stmt}"
        );
    }

    #[test]
    fn the_monitor_role_gets_pg_monitor_not_data_access() {
        let sql = roles().statements().unwrap().join("\n");
        assert!(sql.contains("GRANT pg_monitor TO \"pgpod_monitor\""));
        assert!(
            !sql.contains("GRANT ALL"),
            "probes must not get data access"
        );
    }

    #[test]
    fn a_quote_in_a_password_cannot_escape_the_literal() {
        // The realistic injection: a generated or user-supplied password
        // containing a single quote.
        let mut r = roles();
        r.superuser_password = Secret::new("pw'; DROP DATABASE appdb; --");
        let sql = r.statements().unwrap().join("\n");
        assert!(
            sql.contains("pw''; DROP DATABASE appdb; --"),
            "not doubled:\n{sql}"
        );
        // The unescaped form — a single quote closing the literal right
        // before the payload — is what an injection would look like.
        // Checking for "'; DROP" alone would false-positive, since that
        // sequence also occurs *inside* the correctly doubled literal.
        assert!(
            !sql.contains("pw'; DROP"),
            "injection escaped the literal:\n{sql}"
        );
        // Every statement must have balanced quoting: an odd count means
        // a literal was left open.
        for stmt in r.statements().unwrap() {
            assert_eq!(
                stmt.matches('\'').count() % 2,
                0,
                "unbalanced quoting leaves a literal open:\n{stmt}"
            );
        }
    }

    #[test]
    fn identifiers_from_a_manifest_are_validated_not_just_quoted() {
        for bad in [
            "app db",
            "App",
            "app\"; DROP DATABASE x; --",
            "1app",
            "",
            &"x".repeat(64),
        ] {
            let mut r = roles();
            r.app = Some(AppDatabase {
                database: bad.to_string(),
                owner: "app".into(),
                owner_password: Secret::new("p"),
            });
            assert!(
                matches!(r.statements(), Err(Error::InvalidIdentifier(_))),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn no_app_database_means_no_create_database() {
        // A restored or cloned instance already has its databases.
        let mut r = roles();
        r.app = None;
        assert_eq!(r.app_database_sql().unwrap(), None);
        let sql = r.statements().unwrap().join("\n");
        assert!(sql.contains(REPLICATION_ROLE), "roles are still created");
    }

    #[test]
    fn database_creation_is_a_check_plus_a_create_not_a_psql_metacommand() {
        // `\gexec` is a psql meta-command; through `psql -c` or a driver
        // it is a no-op and the database would silently never appear.
        let (check, create) = roles().app_database_sql().unwrap().unwrap();
        assert!(check.contains("pg_database"), "{check}");
        assert!(check.contains("'appdb'"), "{check}");
        assert!(create.starts_with("CREATE DATABASE \"appdb\""), "{create}");
        assert!(create.contains("OWNER \"app\""), "{create}");
        for stmt in [&check, &create] {
            assert!(!stmt.contains("gexec"), "no meta-commands: {stmt}");
        }
    }

    #[test]
    fn a_quoted_database_name_cannot_break_out_of_the_create() {
        let mut r = roles();
        r.app = Some(AppDatabase {
            database: "app\"; DROP DATABASE x; --".into(),
            owner: "app".into(),
            owner_password: Secret::new("p"),
        });
        assert!(matches!(
            r.app_database_sql(),
            Err(Error::InvalidIdentifier(_))
        ));
    }

    #[test]
    fn role_creation_is_idempotent() {
        // Bootstrap can die partway and be re-run.
        let sql = roles().statements().unwrap().join("\n");
        assert!(
            sql.contains("IF NOT EXISTS"),
            "role creation must be guarded"
        );
        let (check, _) = roles().app_database_sql().unwrap().unwrap();
        assert!(
            check.contains("SELECT 1 FROM pg_database"),
            "database creation must be guarded by an existence check"
        );
    }

    #[test]
    fn passwords_never_appear_in_debug_output() {
        // BootstrapRoles is exactly the sort of struct that ends up in an
        // error context or a tracing span.
        let rendered = format!("{:?}", roles());
        for pw in ["su-pw", "repl-pw", "mon-pw", "app-pw"] {
            assert!(!rendered.contains(pw), "leaked {pw}: {rendered}");
        }
    }

    #[test]
    fn quoting_helpers_double_the_right_character() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("a'b"), "'a''b'");
    }
}
