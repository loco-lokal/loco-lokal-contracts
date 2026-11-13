# frozen_string_literal: true

# Seeds the Rails reference database with data for the API parity checks (run through
# rust/scripts/parity/seed.sh). Idempotent: an existing "Parity NDA" template is reused.
#
# Prints PARITY_TOKEN (the first user's API token) and PARITY_TEMPLATE_ID.
#
# With a second argument `combined`, seeds a separate "Parity Combined" account with
# `combine_pdf_result_key` enabled instead (for combined.py), so that the setting does
# not change what the other checks see in the first account.

user = User.order(:id).first || abort('Complete the first-run setup first.')

if ARGV[1] == 'combined'
  account = Account.find_by(name: 'Parity Combined') ||
            Account.create!(name: 'Parity Combined', timezone: user.account.timezone, locale: user.account.locale)

  user = account.users.find_by(email: 'parity-combined@example.com') ||
         account.users.create!(email: 'parity-combined@example.com', first_name: 'Parity', last_name: 'Combined',
                               password: SecureRandom.hex(16), role: 'admin')

  account.account_configs.find_or_initialize_by(key: AccountConfig::COMBINE_PDF_RESULT_KEY).update!(value: true)
end

token = user.access_token || user.create_access_token!

template = user.account.templates.find_by(name: 'Parity NDA')

unless template
  template = Template.create!(
    account: user.account,
    author: user,
    name: 'Parity NDA',
    external_id: 'parity-nda',
    folder: TemplateFolders.find_or_create_by_name(user, 'Parity / Contracts')
  )

  pdf = File.binread(ARGV.fetch(0))
  tempfile = Tempfile.new(['sample-document', '.pdf']).tap { |f| f.binmode.write(pdf) && f.rewind }
  file = ActionDispatch::Http::UploadedFile.new(tempfile:, filename: 'sample-document.pdf',
                                                type: 'application/pdf')

  documents, = Templates::CreateAttachments.call(template, { files: [file] })
  document = documents.first

  first_party = { 'name' => 'First Party', 'uuid' => SecureRandom.uuid }
  second_party = { 'name' => 'Second Party', 'uuid' => SecureRandom.uuid }

  area = lambda do |x, y, w, h|
    [{ 'x' => x, 'y' => y, 'w' => w, 'h' => h, 'attachment_uuid' => document.uuid, 'page' => 0 }]
  end

  template.update!(
    schema: [{ 'attachment_uuid' => document.uuid, 'name' => document.filename.base }],
    submitters: [first_party, second_party],
    fields: [
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => first_party['uuid'], 'name' => 'Full Name',
        'type' => 'text', 'required' => true, 'areas' => area.call(0.1, 0.1, 0.3, 0.04) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => first_party['uuid'], 'name' => 'Start Date',
        'type' => 'date', 'required' => false, 'areas' => area.call(0.1, 0.2, 0.2, 0.04) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => first_party['uuid'], 'name' => 'Plan',
        'type' => 'select', 'required' => false,
        'options' => [{ 'uuid' => SecureRandom.uuid, 'value' => 'Basic' },
                      { 'uuid' => SecureRandom.uuid, 'value' => 'Pro' }],
        'areas' => area.call(0.1, 0.3, 0.2, 0.04) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => first_party['uuid'], 'name' => 'Signature',
        'type' => 'signature', 'required' => true, 'areas' => area.call(0.1, 0.8, 0.3, 0.08) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => second_party['uuid'], 'name' => 'Approver',
        'type' => 'text', 'required' => true, 'areas' => area.call(0.6, 0.1, 0.3, 0.04) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => second_party['uuid'], 'name' => 'Countersignature',
        'type' => 'signature', 'required' => true, 'areas' => area.call(0.6, 0.8, 0.3, 0.08) }
    ]
  )
end

puts "PARITY_TOKEN=#{token.token}"
puts "PARITY_TEMPLATE_ID=#{template.id}"
