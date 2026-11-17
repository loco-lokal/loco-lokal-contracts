# frozen_string_literal: true

# Prepares previews.py (run through `bin/rails runner`): a user who signs in with a known
# password, in the first user's account, a template with an 18-page PDF (form fields, a
# rotated page, an image, a form XObject) and a small second document, and a template of
# another account. Idempotent: the templates are reused. Prints one JSON object:
# `{ "email":, "password":, "template_id":, "other_template_id": }`.

require 'hexapdf'

def save(doc)
  io = StringIO.new
  doc.write(io, optimize: false, validate: false)
  io.string
end

def image_file
  path = Rails.root.join('tmp/parity-previews.png').to_s
  Vips::Image.black(120, 80, bands: 3).new_from_image([30, 90, 200])
             .draw_rect([250, 200, 20], 20, 20, 50, 30, fill: true).write_to_file(path)
  path
end

def add_first_page(doc, form)
  page = doc.pages.add
  canvas = page.canvas
  canvas.font('Helvetica', size: 14).text('Previews and page objects', at: [50, 760])
  canvas.font('Helvetica', size: 10).text('Second line, smaller', at: [50, 740])
  canvas.font('Helvetica', size: 10).text('Same line', at: [300, 741])
  canvas.image(image_file, at: [50, 560], width: 120, height: 80)

  xobject = doc.add({ Type: :XObject, Subtype: :Form, BBox: [0, 0, 200, 40] })
  xobject.canvas.font('Helvetica', size: 12).text('Inside a form XObject', at: [5, 15])
  canvas.xobject(xobject, at: [300, 500])

  field = form.create_text_field('Full Name')
  field.create_widget(page, Rect: [50, 680, 250, 700])
  field.field_value = 'Filled value'
end

def add_rotated_page(doc)
  page = doc.pages.add
  page.rotate(90)
  page.canvas.font('Helvetica', size: 12).text('Rotated page', at: [100, 700])
  page.canvas.fill_color('red').rectangle(100, 100, 50, 50).fill
end

def pages_pdf
  doc = HexaPDF::Document.new
  form = doc.acro_form(create: true)
  add_first_page(doc, form)
  add_rotated_page(doc)

  16.times do |index|
    page = doc.pages.add
    page.canvas.fill_color(index.even? ? 'red' : 'blue').rectangle(50, 50, 100, 100).fill
    page.canvas.font('Helvetica', size: 12).text("Page #{index + 3}", at: [50, 700])
  end

  form.create_appearances
  save(doc)
end

def small_pdf
  doc = HexaPDF::Document.new
  doc.pages.add([0, 0, 595, 842]).canvas.font('Helvetica', size: 12).text('Second document', at: [50, 800])
  save(doc)
end

def upload(template, name, data)
  tempfile = Tempfile.new([name, '.pdf']).tap { |f| f.binmode.write(data) && f.rewind }
  file = ActionDispatch::Http::UploadedFile.new(tempfile:, filename: "#{name}.pdf", type: 'application/pdf')

  Templates::CreateAttachments.call(template, { files: [file] }).first.first
end

def create_template(user)
  template = Template.create!(account: user.account, author: user, name: 'Parity Previews',
                              folder: TemplateFolders.find_or_create_by_name(user, 'Parity / Previews'))

  documents = [upload(template, 'pages', pages_pdf), upload(template, 'second', small_pdf)]
  party = { 'name' => 'First Party', 'uuid' => SecureRandom.uuid }

  template.update!(
    schema: documents.map { |d| { 'attachment_uuid' => d.uuid, 'name' => d.filename.base } },
    submitters: [party],
    fields: [{ 'uuid' => SecureRandom.uuid, 'submitter_uuid' => party['uuid'], 'name' => 'Name', 'type' => 'text',
               'areas' => [{ 'x' => 0.1, 'y' => 0.1, 'w' => 0.3, 'h' => 0.04,
                             'attachment_uuid' => documents.first.uuid, 'page' => 0 }] }]
  )
  template
end

def other_template
  account = Account.find_or_create_by!(name: 'Parity Previews Other')
  user = account.users.find_or_initialize_by(email: 'parity-previews-other@example.com')
  user.assign_attributes(first_name: 'Other', last_name: 'Owner', role: 'admin', password: SecureRandom.hex)
  user.save!

  account.templates.find_by(name: 'Parity Previews Other') ||
    Template.create!(account:, author: user, name: 'Parity Previews Other',
                     folder: TemplateFolders.find_or_create_by_name(user, 'Default'))
end

account = User.order(:id).first.account
user = account.users.find_or_initialize_by(email: 'parity-previews@example.com')
user.assign_attributes(first_name: 'Parity', last_name: 'Previews', role: 'admin',
                       password: 'parity-previews-password', archived_at: nil)
user.save!

# In a transaction, so that a failed run doesn't leave a template without its documents.
template = account.templates.find_by(name: 'Parity Previews', archived_at: nil) ||
           Template.transaction { create_template(user) }

puts JSON.generate(email: user.email, password: 'parity-previews-password',
                   template_id: template.id, other_template_id: other_template.id)
