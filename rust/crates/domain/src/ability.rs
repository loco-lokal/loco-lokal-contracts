//! Port of `lib/ability.rb` (CanCanCan): what a signed-in user may do.
//!
//! Every rule scopes records to the user's account or to the user, so queries should also
//! filter by `account_id` / `user_id` (see the `scope_*` helpers); these checks are for
//! single records. Templates are the exception: a linked (testing) account may use
//! templates shared with it through `template_sharings`.

/// `TemplateSharing::ALL_ID`: a sharing with every linked account.
pub const SHARING_ALL_ID: i64 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Read,
    Create,
    Update,
    Destroy,
    Manage,
}

/// The user an ability is built for.
#[derive(Clone, Copy, Debug)]
pub struct Ability {
    pub user_id: i64,
    pub account_id: i64,
    /// `user.account.linked_account_account.present?`.
    pub linked_account: bool,
}

/// A `template_sharings` row: `(account_id, ability)`.
#[derive(Clone, Debug)]
pub struct TemplateSharing {
    pub account_id: i64,
    pub ability: String,
}

impl Ability {
    /// Records owned by an account: `Submission`, `Submitter`, `User`, `TemplateFolder`,
    /// `EncryptedConfig`, `AccountConfig`, `WebhookUrl`, and `Account` itself
    /// (`account_id` = the account's id). Any action.
    #[must_use]
    pub fn can_account_record(&self, account_id: i64) -> bool {
        account_id == self.account_id
    }

    /// Records owned by a user: `EncryptedUserConfig`, `UserConfig`, `AccessToken`,
    /// `McpToken`. Any action.
    #[must_use]
    pub fn can_user_record(&self, user_id: i64) -> bool {
        user_id == self.user_id
    }

    /// `TemplateSharing` belongs to a template of the user's account.
    #[must_use]
    pub fn can_template_sharing(&self, template_account_id: i64) -> bool {
        template_account_id == self.account_id
    }

    /// `Template`: read/create/update follow `Abilities::TemplateConditions.entity` with
    /// `ability: 'manage'`; destroy needs the user's own account.
    #[must_use]
    pub fn can_template(
        &self,
        action: Action,
        template_account_id: Option<i64>,
        sharings: &[TemplateSharing],
    ) -> bool {
        match action {
            Action::Destroy | Action::Manage => template_account_id == Some(self.account_id),
            Action::Read | Action::Create | Action::Update => {
                self.template_entity(template_account_id, sharings, Some("manage"))
            }
        }
    }

    /// `Abilities::TemplateConditions.entity(template, user:, ability:)`.
    #[must_use]
    pub fn template_entity(
        &self,
        template_account_id: Option<i64>,
        sharings: &[TemplateSharing],
        ability: Option<&str>,
    ) -> bool {
        let Some(account_id) = template_account_id else {
            return true;
        };
        if account_id == self.account_id {
            return true;
        }
        if !self.linked_account || sharings.is_empty() {
            return false;
        }

        sharings.iter().any(|sharing| {
            (sharing.account_id == self.account_id || sharing.account_id == SHARING_ALL_ID)
                && ability.is_none_or(|a| sharing.ability == "manage" || sharing.ability == a)
        })
    }

    /// `can :manage, :mcp`.
    #[must_use]
    pub fn can_mcp(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Ability = Ability {
        user_id: 1,
        account_id: 10,
        linked_account: false,
    };

    fn sharing(account_id: i64, ability: &str) -> TemplateSharing {
        TemplateSharing {
            account_id,
            ability: ability.to_owned(),
        }
    }

    #[test]
    fn scopes_records_to_account_and_user() {
        assert!(USER.can_account_record(10));
        assert!(!USER.can_account_record(11));
        assert!(USER.can_user_record(1));
        assert!(!USER.can_user_record(2));
    }

    #[test]
    fn templates_follow_template_conditions() {
        assert!(USER.can_template(Action::Read, Some(10), &[]));
        assert!(USER.can_template(Action::Destroy, Some(10), &[]));
        assert!(USER.can_template(Action::Read, None, &[]));
        assert!(!USER.can_template(Action::Destroy, None, &[]));
        // Sharings only count for linked accounts.
        assert!(!USER.can_template(Action::Read, Some(20), &[sharing(10, "manage")]));

        let linked = Ability {
            linked_account: true,
            ..USER
        };
        assert!(linked.can_template(Action::Read, Some(20), &[sharing(10, "manage")]));
        assert!(linked.can_template(
            Action::Update,
            Some(20),
            &[sharing(SHARING_ALL_ID, "manage")]
        ));
        assert!(!linked.can_template(Action::Read, Some(20), &[sharing(10, "read")]));
        assert!(!linked.can_template(Action::Read, Some(20), &[sharing(30, "manage")]));
        assert!(!linked.can_template(Action::Destroy, Some(20), &[sharing(10, "manage")]));
        assert!(linked.template_entity(Some(20), &[sharing(10, "read")], None));
    }
}
