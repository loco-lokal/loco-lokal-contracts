# frozen_string_literal: true

# Seeds the Rails reference database with data for the API parity checks (run through
# rust/scripts/parity/seed.sh). Idempotent: an existing "Parity NDA" template is reused.
#
# Prints PARITY_TOKEN (the first user's API token) and PARITY_TEMPLATE_ID.

user = User.order(:id).first || abort('Complete the first-run setup first.')
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

# A single-party template with two image documents, whose results Rails also merges into one
# PDF. The second image is the first one mirrored, so that the page order shows.
unless user.account.templates.exists?(name: 'Parity Images')
  images = Template.create!(account: user.account, author: user, name: 'Parity Images',
                            external_id: 'parity-images', folder: template.folder)

  png = File.binread(ARGV.fetch(1))
  mirrored = Vips::Image.new_from_buffer(png, '').fliphor.write_to_buffer('.png')
  documents = [['front.png', png], ['back.png', mirrored]].map do |filename, data|
    tempfile = Tempfile.new(['image', '.png']).tap { |f| f.binmode.write(data) && f.rewind }
    file = ActionDispatch::Http::UploadedFile.new(tempfile:, filename:, type: 'image/png')

    Templates::CreateAttachments.call(images, { files: [file] }).first.first
  end

  signer = { 'name' => 'Signer', 'uuid' => SecureRandom.uuid }
  area = lambda do |document, x, y, w, h|
    [{ 'x' => x, 'y' => y, 'w' => w, 'h' => h, 'attachment_uuid' => document.uuid, 'page' => 0 }]
  end

  images.update!(
    schema: documents.map { |d| { 'attachment_uuid' => d.uuid, 'name' => d.filename.base } },
    submitters: [signer],
    fields: [
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => signer['uuid'], 'name' => 'Full Name',
        'type' => 'text', 'required' => true, 'areas' => area.call(documents[0], 0.1, 0.1, 0.4, 0.05) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => signer['uuid'], 'name' => 'Signature',
        'type' => 'signature', 'required' => true, 'areas' => area.call(documents[1], 0.5, 0.8, 0.4, 0.1) }
    ]
  )
end

# Single-party templates whose PDFs have interactive forms, so the results generate field
# appearances and flatten them as HexaPDF does: "Parity Form" has no appearances and sets
# `NeedAppearances`; "Parity Form Stale" keeps HexaPDF's appearances (some stale) and leaves
# out a few, without `NeedAppearances`.
[['Parity Form', 'parity-form', true], ['Parity Form Stale', 'parity-form-stale', false]].each do |name, external_id, need|
  next if user.account.templates.exists?(name:)

  pdf = HexaPDF::Document.new
  page = pdf.pages.add([0, 0, 595, 842])
  page.canvas.font('Helvetica', size: 14).text('Parity form', at: [50, 800])
  acro_form = pdf.acro_form(create: true)
  acro_form.set_default_appearance_string(font_size: 0)

  text = lambda do |name, rect, value = nil, **options, &block|
    field = acro_form.create_text_field(name)
    block&.call(field)
    widget = field.create_widget(page, Rect: rect)
    widget.border_style(**options[:border]) if options[:border]
    widget.background_color(options[:background]) if options[:background]
    (widget[:MK] ||= {})[:R] = options[:rotate] if options[:rotate]
    field[:DA] = options[:da] if options[:da]
    field[:Q] = options[:align] if options[:align]
    field.field_value = value if value
    field
  end
  text.call('name', [50, 740, 250, 760], 'Jane Doe')
  text.call('auto', [300, 740, 400, 752], 'A rather long value that has to shrink')
  text.call('notes', [50, 640, 250, 720], "First line\nSecond line that wraps around the box", da: '0 g /F1 9 Tf') do |f|
    f.initialize_as_multiline_text_field
  end
  text.call('code', [300, 690, 420, 710], '123456', da: '0 0 1 rg /F1 12 Tf') do |f|
    f[:MaxLen] = 6
    f.initialize_as_comb_text_field
  end
  text.call('centered', [300, 650, 500, 675], 'Centered', align: 1, da: '/F1 11 Tf 1 0 0 rg',
                                                            border: { color: [0, 0, 1], width: 2 },
                                                            background: [0.9, 0.9, 0.6])
  text.call('rotated', [50, 520, 75, 620], 'Turned', rotate: 90, da: '/F1 10 Tf 0 g')
  text.call('empty', [300, 600, 500, 620], nil, border: { color: 0 })
  text.call('amount', [300, 560, 500, 580], '-1234.5', da: '/F1 10 Tf 0 g') do |f|
    f.set_format_action(:number, decimals: 2, separator_style: :point, negative_style: :parens_red,
                                 currency_string: '$')
  end
  text.call('missing font', [300, 520, 500, 540], 'Fallback', da: '/Missing 10 Tf 0 g')

  check = acro_form.create_check_box('agree')
  check.create_widget(page, Rect: [50, 480, 66, 496])
  check.field_value = true
  check_off = acro_form.create_check_box('decline')
  check_off.create_widget(page, Rect: [80, 480, 96, 496])
  check_off.field_value = false
  radio = acro_form.create_radio_button('choice')
  radio.create_widget(page, Rect: [120, 480, 136, 496], value: :one)
  radio.create_widget(page, Rect: [140, 480, 156, 496], value: :two)
  radio.field_value = :two

  combo = acro_form.create_combo_box('plan')
  combo.option_items = %w[Basic Pro Enterprise]
  combo.create_widget(page, Rect: [300, 470, 450, 490])
  combo.field_value = 'Pro'
  list = acro_form.create_list_box('colors')
  list.option_items = %w[Red Green Blue Yellow]
  list.create_widget(page, Rect: [50, 380, 200, 460])
  list.field_value = 'Blue'

  kept = text.call('kept', [300, 420, 500, 440], 'Kept appearance')
  if need
    acro_form.each_field do |field|
      next if field == kept

      field.each_widget do |widget|
        if field == check
          widget[:AP][:N].delete(:Off) # only the on state: regenerated
        else
          widget.delete(:AP)
        end
      end
    end
    acro_form[:NeedAppearances] = true
  else
    pdf.config['acro_form.create_appearances'] = false # keep the stale appearances when writing
    kept[:V] = 'Stale value' # the existing appearance is kept
    combo[:V] = 'Enterprise' # choice fields are always regenerated
    check[:AP][:N].delete(:Off)
    %w[notes empty amount].each { |n| acro_form.field_by_name(n).each_widget { |w| w.delete(:AP) } }
  end

  form_template = Template.create!(account: user.account, author: user, name:,
                                   external_id:, folder: template.folder)
  tempfile = Tempfile.new(['form', '.pdf']).tap { |f| f.binmode.write(pdf.write_to_string) && f.rewind }
  file = ActionDispatch::Http::UploadedFile.new(tempfile:, filename: 'form.pdf', type: 'application/pdf')
  document = Templates::CreateAttachments.call(form_template, { files: [file] }).first.first

  signer = { 'name' => 'Signer', 'uuid' => SecureRandom.uuid }
  area = ->(x, y, w, h) { [{ 'x' => x, 'y' => y, 'w' => w, 'h' => h, 'attachment_uuid' => document.uuid, 'page' => 0 }] }
  form_template.update!(
    schema: [{ 'attachment_uuid' => document.uuid, 'name' => document.filename.base }],
    submitters: [signer],
    fields: [
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => signer['uuid'], 'name' => 'Full Name',
        'type' => 'text', 'required' => true, 'areas' => area.call(0.1, 0.7, 0.4, 0.04) },
      { 'uuid' => SecureRandom.uuid, 'submitter_uuid' => signer['uuid'], 'name' => 'Signature',
        'type' => 'signature', 'required' => true, 'areas' => area.call(0.5, 0.8, 0.4, 0.1) }
    ]
  )
end

puts "PARITY_TOKEN=#{token.token}"
puts "PARITY_TEMPLATE_ID=#{template.id}"
