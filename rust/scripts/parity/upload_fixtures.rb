# frozen_string_literal: true

# Builds the PDFs that uploads.py uploads to both servers (run through `bin/rails runner`).
# Prints one JSON object: file name => base64 PDF.

require 'hexapdf'

def save(doc)
  io = StringIO.new
  doc.write(io, optimize: false, validate: false)
  io.string
end

def add_text(doc, page, text, y)
  page.canvas.font('Helvetica', size: 12).text(text, at: [50, y])
  doc
end

def form_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add
  add_text(doc, page, 'Fillable form', 760)
  form = doc.acro_form(create: true)

  name = form.create_text_field('Full Name')
  name.create_widget(page, Rect: [50, 700, 250, 720])
  name.flag(:required)
  name[:TU] = 'Your full name'

  centered = form.create_text_field('Centered')
  centered.create_widget(page, Rect: [300, 700, 500, 720])
  centered[:Q] = 1

  filled = form.create_text_field('Prefilled')
  filled.create_widget(page, Rect: [50, 660, 250, 680])
  filled.field_value = 'Already there'

  code = form.create_comb_text_field('Code', max_chars: 5)
  code.create_widget(page, Rect: [300, 660, 400, 680])

  date = form.create_text_field('Start Date')
  date.create_widget(page, Rect: [50, 620, 250, 640])
  date[:AA] = { F: { S: :JavaScript, JS: 'AFDate_FormatEx("mm/dd/yyyy");' },
                K: { S: :JavaScript, JS: 'AFDate_KeystrokeEx("mm/dd/yyyy");' } }

  agree = form.create_check_box('Agree')
  agree.create_widget(page, Rect: [50, 580, 64, 594])

  plan = form.create_radio_button('Plan')
  plan.create_widget(page, value: :Basic, Rect: [50, 540, 64, 554])
  plan.create_widget(page, value: :Pro, Rect: [100, 540, 114, 554])

  country = form.create_combo_box('Country', option_items: ['Select a country', 'Germany', 'France'])
  country.create_widget(page, Rect: [300, 580, 500, 600])

  size = form.create_combo_box('Size', option_items: %w[Small Large])
  size.create_widget(page, Rect: [300, 540, 500, 560])
  size.field_value = 'Large'

  signature = form.create_signature_field('Signature')
  signature.create_widget(page, Rect: [50, 440, 250, 500])

  initials = form.create_signature_field('Initials')
  initials.create_widget(page, Rect: [300, 440, 400, 500])

  push = form.send(:create_field, 'Reset', :Btn, &:initialize_as_push_button)
  push.create_widget(page, Rect: [450, 440, 500, 460])

  rotated = doc.pages.add
  rotated.rotate(90)
  add_text(doc, rotated, 'Rotated page', 700)
  notes = form.create_text_field('Notes 2')
  notes.create_widget(rotated, Rect: [100, 300, 300, 330])
  rotated[:Annots] ||= []
  rotated[:Annots] << doc.add({ Type: :Annot, Subtype: :Link, Rect: [100, 500, 300, 520],
                                A: { S: :URI, URI: 'https://example.com/rotated' } })

  form.create_appearances
  save(doc)
end

def links_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add([0, 0, 595, 842])
  add_text(doc, page, 'Links', 800)
  page[:Annots] = [
    { URI: 'https://example.com/a', Rect: [50, 700, 250, 720] },
    { URI: 'http://example.com/b', Rect: [50, 650, 250, 670] },
    { URI: 'mailto:someone@example.com', Rect: [50, 600, 250, 620] }
  ].map do |link|
    doc.add({ Type: :Annot, Subtype: :Link, Rect: link[:Rect], A: { S: :URI, URI: link[:URI] } })
  end
  save(doc)
end

def pages_pdf(count)
  doc = HexaPDF::Document.new
  count.times do |index|
    page = doc.pages.add
    page.canvas.fill_color(index.even? ? 'red' : 'blue').rectangle(50, 50, 100, 100).fill
    add_text(doc, page, "Page #{index + 1}", 700)
  end
  save(doc)
end

def encrypted_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add
  add_text(doc, page, 'Encrypted', 700)
  doc.encrypt(user_password: 'secret', owner_password: 'owner-secret')
  save(doc)
end

puts JSON.generate(
  {
    'form.pdf' => form_pdf,
    'links.pdf' => links_pdf,
    'pages.pdf' => pages_pdf(18),
    'encrypted.pdf' => encrypted_pdf
  }.transform_values { |data| Base64.strict_encode64(data) }
)
