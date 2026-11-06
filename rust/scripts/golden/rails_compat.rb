# frozen_string_literal: true

# Prints golden values for the Rust `rails-compat` crate as JSON.
# Run inside the Rails reference container (see rust/scripts/generate-golden.sh).

require 'json'

enc = ActiveRecord::Encryption
cfg = enc.config

plaintexts = {
  'short' => 'https://example.com/webhook',
  'empty' => '',
  'unicode' => 'Grüße – 署名 ✍️',
  'long' => ('Lorem ipsum dolor sit amet, consectetur adipiscing elit. ' * 6)
}

encryptor = enc::Encryptor.new
encryption = plaintexts.transform_values { |text| { 'plaintext' => text, 'ciphertext' => encryptor.encrypt(text) } }

derived_primary = enc.key_provider.encryption_key.secret

verifier = ApplicationRecord.signed_id_verifier
far = 100.years
persisted = ->(klass, id) { klass.new(id:).tap { |r| r.instance_variable_set(:@new_record, false) } }

signed = {
  'submitter_download_completed' => {
    'data' => 42,
    'purpose' => 'submitter/download_completed',
    'message' => persisted.call(Submitter, 42).signed_id(expires_in: far, purpose: :download_completed)
  },
  'attachment_uuid' => {
    'data' => 'b1c2d3e4-0000-4000-8000-000000000001',
    'purpose' => 'attachment',
    'message' => verifier.generate('b1c2d3e4-0000-4000-8000-000000000001', expires_in: far, purpose: :attachment)
  },
  'attachment_pair' => {
    'data' => [7, 'b1c2d3e4-0000-4000-8000-000000000001'],
    'purpose' => 'attachment',
    'message' => verifier.generate([7, 'b1c2d3e4-0000-4000-8000-000000000001'], expires_in: far, purpose: :attachment)
  },
  'blob_proxy_no_purpose' => {
    'data' => ['b1c2d3e4-0000-4000-8000-000000000001', 'blob', 4_102_444_800],
    'purpose' => nil,
    'message' => verifier.generate(['b1c2d3e4-0000-4000-8000-000000000001', 'blob', 4_102_444_800])
  },
  'expired' => {
    'data' => 1,
    'purpose' => 'attachment',
    'message' => verifier.generate(1, expires_at: Time.utc(2001, 1, 1), purpose: :attachment)
  }
}

active_storage_blob = {
  'data' => 7,
  'purpose' => 'blob_id',
  'message' => persisted.call(ActiveStorage::Blob, 7).signed_id
}

smtp_value = {
  'host' => 'mail.example.test', 'port' => '465', 'username' => 'contracts',
  'password' => 'smtp-secret', 'from_email' => 'contracts@example.test'
}
# Exactly what Rails writes to encrypted_configs.value (JSON-serialized, then encrypted).
encrypted_config = {
  'value' => smtp_value,
  'stored' => EncryptedConfig.type_for_attribute(:value).serialize(smtp_value)
}

password = 'correct horse battery staple'
user = User.new(password:)

puts JSON.pretty_generate(
  'secret_key_base' => Rails.application.secret_key_base,
  'encryption' => {
    'primary_key' => cfg.primary_key,
    'key_derivation_salt' => cfg.key_derivation_salt,
    'hash_digest_class' => cfg.hash_digest_class.name,
    'derived_primary_key_hex' => derived_primary.unpack1('H*'),
    'values' => encryption
  },
  'signed_id' => {
    'secret_hex' => Rails.application.message_verifiers['active_record/signed_id'].instance_variable_get(:@secret).unpack1('H*'),
    'values' => signed
  },
  'active_storage' => {
    'secret_hex' => ActiveStorage.verifier.instance_variable_get(:@secret).unpack1('H*'),
    'blob_signed_id' => active_storage_blob
  },
  'encrypted_config_smtp' => encrypted_config,
  'bcrypt' => { 'password' => password, 'hash' => user.encrypted_password, 'pepper' => Devise.pepper }
)
