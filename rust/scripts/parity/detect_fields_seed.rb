# frozen_string_literal: true

# Prepares detect_fields.py (run through `bin/rails runner`): a user who signs in with a
# known password, in the first user's account, and a template whose documents exercise
# `Templates::DetectFields`:
#
# - `form`: a form with underscore blanks after date, signature, number and plain labels
#   (English, German and French), ruled lines, a table, check boxes and a filled
#   AcroForm field; a rotated page; a blank page; a page with little content (scaled up
#   for the model); a landscape page; and a page of only underscores and lines.
# - `letter`: an A4 page with a few blanks at the bottom.
# - `form-image`: the first page of `form` rendered to a PNG, uploaded as an image.
# - `tall-image`: a tall PNG (its preview is reduced with an integer shrink first).
#
# Also a template of another account, and one whose image document has a grayscale
# preview. Idempotent: the templates are reused. Prints one JSON object:
# `{ "email":, "password":, "template_id":, "documents": { name => uuid },
#    "other_template_id":, "unsupported": { "template_id": } }`.

require 'hexapdf'

def save(doc)
  io = StringIO.new
  doc.write(io, optimize: false, validate: false)
  io.string
end

def blank(canvas, label, left, bottom, underscores: 30, size: 11, font: 'Helvetica')
  canvas.font(font, size:).text("#{label} #{'_' * underscores}", at: [left, bottom])
end

def ruled(canvas, label, left, bottom, length: 200, size: 11)
  canvas.font('Helvetica', size:).text(label, at: [left, bottom])
  width = canvas.font.decode_utf8(label).sum { |g| g.width * size / 1000.0 }
  canvas.line_width(0.8).line(left + width + 6, bottom - 2, left + width + 6 + length, bottom - 2).stroke
end

def checkbox(canvas, label, left, bottom, size: 10)
  canvas.line_width(0.8).rectangle(left, bottom - 1, size, size).stroke
  canvas.font('Helvetica', size: 11).text(label, at: [left + size + 5, bottom])
end

def form_blanks(canvas)
  canvas.font('Helvetica', variant: :bold, size: 16).text('Application Form', at: [50, 750])
  canvas.font('Helvetica', size: 10).text('Please fill in all fields in block capitals.', at: [50, 732])

  blank(canvas, 'Full Name:', 50, 700, underscores: 45)
  blank(canvas, 'Date:', 50, 675, underscores: 20)
  blank(canvas, 'Phone', 300, 675, underscores: 22)
  blank(canvas, 'Total $', 50, 650, underscores: 15)
  blank(canvas, 'Quantity:', 300, 650, underscores: 12)
  ruled(canvas, 'Email', 50, 620, length: 230)
  ruled(canvas, 'Address', 50, 595, length: 420)
end

def form_options(canvas)
  canvas.font('Helvetica', variant: :bold, size: 12).text('Options', at: [50, 560])
  checkbox(canvas, 'Yes', 50, 540)
  checkbox(canvas, 'No', 110, 540)
  checkbox(canvas, 'Maybe later', 170, 540)
  checkbox(canvas, 'I agree to the terms and conditions', 50, 520)
end

# A table: header row, three rows to fill in.
def form_table(canvas)
  top = 480
  canvas.line_width(0.6)
  [50, 250, 400, 550].each { |x| canvas.line(x, top, x, top - 100).stroke }
  (0..4).each { |i| canvas.line(50, top - (i * 25), 550, top - (i * 25)).stroke }
  canvas.font('Helvetica', variant: :bold, size: 10)
  canvas.text('Item', at: [55, top - 17])
  canvas.text('Menge', at: [255, top - 17])
  canvas.text('Preis', at: [405, top - 17])
end

def form_page(doc, form)
  page = doc.pages.add
  canvas = page.canvas
  form_blanks(canvas)
  form_options(canvas)
  form_table(canvas)

  blank(canvas, 'Unterschrift:', 50, 330, underscores: 25)
  blank(canvas, 'Datum', 330, 330, underscores: 18)
  blank(canvas, 'Signez ici', 50, 300, underscores: 25)
  blank(canvas, 'Prix', 330, 300, underscores: 18)

  field = form.create_text_field('Reference')
  field.create_widget(page, Rect: [50, 240, 250, 262])
  field.field_value = 'REF-1234'
  canvas.font('Helvetica', size: 9).text('Reference (prefilled)', at: [50, 266])

  ruled(canvas, 'Signature', 50, 150, length: 180)
  ruled(canvas, 'Signed at', 320, 150, length: 160)
  canvas.font('Helvetica', size: 8).text('Page 1 of 6', at: [280, 40])
end

def rotated_page(doc)
  page = doc.pages.add
  page.rotate(90)
  canvas = page.canvas
  blank(canvas, 'Name:', 60, 700, underscores: 30)
  blank(canvas, 'Signature:', 60, 660, underscores: 30)
  checkbox(canvas, 'Accept', 60, 620)
end

def small_page(doc)
  canvas = doc.pages.add.canvas
  blank(canvas, 'Initials', 220, 420, underscores: 12, size: 10)
  checkbox(canvas, 'Done', 220, 395, size: 9)
end

def landscape_page(doc)
  canvas = doc.pages.add([0, 0, 842, 595]).canvas
  canvas.font('Helvetica', variant: :bold, size: 14).text('Landscape sheet', at: [40, 550])
  4.times do |i|
    blank(canvas, "Participant #{i + 1}", 40, 500 - (i * 40), underscores: 35)
    ruled(canvas, 'Sign', 480, 500 - (i * 40), length: 200)
  end
end

def lines_page(doc)
  canvas = doc.pages.add.canvas
  canvas.font('Courier', size: 12)
  canvas.text('_' * 40, at: [60, 700])
  canvas.text("#{'_' * 15}     #{'_' * 15}", at: [60, 660])
  canvas.line_width(1)
  canvas.line(60, 600, 400, 600).stroke
  canvas.line(60, 560, 560, 560).stroke
  canvas.line(300, 540, 300, 470).stroke
  canvas.rectangle(60, 400, 200, 40).stroke
end

def form_pdf
  doc = HexaPDF::Document.new
  form = doc.acro_form(create: true)
  form_page(doc, form)
  rotated_page(doc)
  doc.pages.add
  small_page(doc)
  landscape_page(doc)
  lines_page(doc)
  form.create_appearances
  save(doc)
end

def letter_pdf
  doc = HexaPDF::Document.new
  canvas = doc.pages.add([0, 0, 595, 842]).canvas
  canvas.font('Times', size: 12)
  [
    'Dear Sir or Madam,',
    'We confirm the order of 12 units at the agreed price.',
    'Please return a signed copy of this letter.'
  ].each_with_index { |line, i| canvas.text(line, at: [70, 760 - (i * 18)]) }
  blank(canvas, 'Signature', 70, 200, underscores: 28, font: 'Times')
  blank(canvas, 'Date', 330, 200, underscores: 18, font: 'Times')
  save(doc)
end

def form_png(pdf)
  doc = Pdfium::Document.open_bytes(pdf)
  page = doc.get_page(0)
  data, width, height = page.render_to_bitmap(width: 1200)
  Vips::Image.new_from_memory_copy(data, width, height, 4, :uchar).extract_band(0, n: 3).write_to_buffer('.png')
ensure
  page&.close
  doc&.close
end

def tall_png
  image = Vips::Image.black(700, 2400, bands: 3).new_from_image([255, 255, 255])
  6.times do |i|
    image = image.draw_rect([0, 0, 0], 60, 200 + (i * 350), 500, 3, fill: true)
    image = image.draw_rect([0, 0, 0], 600, 170 + (i * 350), 30, 30)
  end
  image.write_to_buffer('.png')
end

def upload(template, name, data, type)
  ext = type == 'application/pdf' ? '.pdf' : '.png'
  tempfile = Tempfile.new([name, ext]).tap { |f| f.binmode.write(data) && f.rewind }
  file = ActionDispatch::Http::UploadedFile.new(tempfile:, filename: "#{name}#{ext}", type:)

  Templates::CreateAttachments.call(template, { files: [file] }).first.first
end

def create_template(user)
  template = Template.create!(account: user.account, author: user, name: 'Parity Detect Fields',
                              folder: TemplateFolders.find_or_create_by_name(user, 'Parity / Detect'))
  form = form_pdf
  documents = [
    upload(template, 'form', form, 'application/pdf'),
    upload(template, 'letter', letter_pdf, 'application/pdf'),
    upload(template, 'form-image', form_png(form), 'image/png'),
    upload(template, 'tall-image', tall_png, 'image/png')
  ]
  party = { 'name' => 'First Party', 'uuid' => SecureRandom.uuid }

  template.update!(
    schema: documents.map { |d| { 'attachment_uuid' => d.uuid, 'name' => d.filename.base } },
    submitters: [party]
  )
  template
end

# An image document whose preview is a grayscale PNG (which Rails' uploads don't make):
# the Rust server answers 422 for it.
def unsupported_template(user)
  template = Template.create!(account: user.account, author: user, name: 'Parity Detect Unsupported',
                              folder: TemplateFolders.find_or_create_by_name(user, 'Parity / Detect'))
  document = upload(template, 'gray', tall_png, 'image/png')
  document.preview_images.each(&:purge)
  gray = Vips::Image.black(300, 200).new_from_image(255).cast(:uchar).draw_rect(0, 40, 100, 200, 3, fill: true)
  ActiveStorage::Attachment.create!(
    blob: ActiveStorage::Blob.create_and_upload!(io: StringIO.new(gray.write_to_buffer('.png')), filename: '0.png',
                                                 metadata: { analyzed: true, identified: true, width: 300,
                                                             height: 200 }),
    name: 'preview_images', record: document
  )
  template.update!(schema: [{ 'attachment_uuid' => document.uuid, 'name' => 'gray' }],
                   submitters: [{ 'name' => 'First Party', 'uuid' => SecureRandom.uuid }])
  template
end

def other_template
  account = Account.find_or_create_by!(name: 'Parity Detect Other')
  user = account.users.find_or_initialize_by(email: 'parity-detect-other@example.com')
  user.assign_attributes(first_name: 'Other', last_name: 'Owner', role: 'admin', password: SecureRandom.hex)
  user.save!

  account.templates.find_by(name: 'Parity Detect Other') ||
    Template.create!(account:, author: user, name: 'Parity Detect Other',
                     folder: TemplateFolders.find_or_create_by_name(user, 'Default'))
end

account = User.order(:id).first.account
user = account.users.find_or_initialize_by(email: 'parity-detect@example.com')
user.assign_attributes(first_name: 'Parity', last_name: 'Detect', role: 'admin',
                       password: 'parity-detect-password', archived_at: nil)
user.save!

template = account.templates.find_by(name: 'Parity Detect Fields', archived_at: nil) ||
           Template.transaction { create_template(user) }

unsupported = account.templates.find_by(name: 'Parity Detect Unsupported', archived_at: nil) ||
              Template.transaction { unsupported_template(user) }

documents = template.schema_documents.to_h { |d| [d.filename.base, d.uuid] }

puts JSON.generate(email: user.email, password: 'parity-detect-password',
                   template_id: template.id, documents:, other_template_id: other_template.id,
                   unsupported: { template_id: unsupported.id })
