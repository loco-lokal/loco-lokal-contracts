# frozen_string_literal: true

# Prepares uploads.py (run through `bin/rails runner`): a user who signs in with a known
# password, in the first user's account, and the files to upload. Prints one JSON object:
# `{ "email":, "password":, "files": { name => base64 } }`.

require 'hexapdf'

def save(doc)
  io = StringIO.new
  doc.write(io, optimize: false, validate: false)
  io.string
end

def add_text(doc, page, text, top)
  page.canvas.font('Helvetica', size: 12).text(text, at: [50, top])
  doc
end

def add_text_fields(form, page)
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

  form.create_comb_text_field('Code', max_chars: 5).create_widget(page, Rect: [300, 660, 400, 680])

  date = form.create_text_field('Start Date')
  date.create_widget(page, Rect: [50, 620, 250, 640])
  date[:AA] = { F: { S: :JavaScript, JS: 'AFDate_FormatEx("mm/dd/yyyy");' },
                K: { S: :JavaScript, JS: 'AFDate_KeystrokeEx("mm/dd/yyyy");' } }
end

def add_choice_fields(form, page)
  form.create_check_box('Agree').create_widget(page, Rect: [50, 580, 64, 594])

  plan = form.create_radio_button('Plan')
  plan.create_widget(page, value: :Basic, Rect: [50, 540, 64, 554])
  plan.create_widget(page, value: :Pro, Rect: [100, 540, 114, 554])

  form.create_combo_box('Country', option_items: ['Select a country', 'Germany', 'France'])
      .create_widget(page, Rect: [300, 580, 500, 600])

  size = form.create_combo_box('Size', option_items: %w[Small Large])
  size.create_widget(page, Rect: [300, 540, 500, 560])
  size.field_value = 'Large'
end

def add_signature_fields(form, page)
  form.create_signature_field('Signature').create_widget(page, Rect: [50, 440, 250, 500])
  form.create_signature_field('Initials').create_widget(page, Rect: [300, 440, 400, 500])

  push = form.send(:create_field, 'Reset', :Btn, &:initialize_as_push_button)
  push.create_widget(page, Rect: [450, 440, 500, 460])
end

def add_rotated_page(doc, form)
  rotated = doc.pages.add
  rotated.rotate(90)
  add_text(doc, rotated, 'Rotated page', 700)
  form.create_text_field('Notes 2').create_widget(rotated, Rect: [100, 300, 300, 330])
  rotated[:Annots] ||= []
  rotated[:Annots] << doc.add({ Type: :Annot, Subtype: :Link, Rect: [100, 500, 300, 520],
                                A: { S: :URI, URI: 'https://example.com/rotated' } })
end

def form_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add
  add_text(doc, page, 'Fillable form', 760)
  form = doc.acro_form(create: true)

  add_text_fields(form, page)
  add_choice_fields(form, page)
  add_signature_fields(form, page)
  add_rotated_page(doc, form)

  form.create_appearances
  save(doc)
end

# The fields below are written as plain dictionaries, for what HexaPDF's form API does not
# build: several widgets per field, export values, `/Opt`, detached widgets.
def appearance(doc, content = '')
  doc.add({ Type: :XObject, Subtype: :Form, BBox: [0, 0, 10, 10] }, stream: content)
end

def on_off(doc, on)
  { N: { on => appearance(doc, '0 0 m 10 10 l S'), Off: appearance(doc) } }
end

def annotate(page, annot)
  page[:Annots] ||= []
  page[:Annots] << annot
  annot
end

# A field with its widget merged in, listed in the form unless `form` is nil (detached).
def widget_field(doc, form, page, rect, **dict)
  annot = annotate(page, doc.add({ Type: :Annot, Subtype: :Widget, Rect: rect, P: page, **dict }))
  form[:Fields] << annot if form
  annot
end

def parent_field(doc, form, **dict)
  parent = doc.add({ Kids: [], **dict })
  form[:Fields] << parent
  parent
end

# A widget of `parent`, not yet on its page's annotations.
def kid(doc, parent, page, rect, **dict)
  annot = doc.add({ Type: :Annot, Subtype: :Widget, Rect: rect, P: page, Parent: parent, **dict })
  parent[:Kids] << annot
  annot
end

REQUIRED = 1 << 1
RADIO = 1 << 15
COMB = 1 << 24
PASSWORD = 1 << 13

def add_acro_text_fields(doc, form, page, other_page)
  widget_field(doc, form, page, [50, 760, 250, 780], FT: :Tx, T: 'Right Aligned', Q: 2, Ff: REQUIRED)
  widget_field(doc, form, page, [300, 760, 400, 780], FT: :Tx, T: 'Secret', Ff: PASSWORD | COMB, MaxLen: 6)
  widget_field(doc, form, page, [420, 760, 520, 780], FT: :Tx, T: 'Comb No Max', Ff: COMB)
  widget_field(doc, form, page, [50, 730, 250, 750], FT: :Tx, T: 'form1[0].page1[0].name[0]', TU: 'Name on card')
  widget_field(doc, form, page, [300, 730, 400, 750], FT: :Tx, T: 'Same Name', TU: 'Same Name')
  widget_field(doc, form, page, [420, 730, 520, 750], FT: :Tx, T: 'Undefined Tip', TU: 'undefined')
  widget_field(doc, form, page, [50, 700, 150, 720], FT: :Tx, T: '123')
  widget_field(doc, form, page, [170, 700, 270, 720], FT: :Tx, T: 'Größe Ü', TU: 'Kleidergröße')
  keystroke = { S: :JavaScript, JS: 'AFDate_KeystrokeEx("dd.mm.yyyy");' }
  widget_field(doc, form, page, [300, 700, 400, 720], FT: :Tx, T: 'Birthday', AA: { K: keystroke })
  format = { S: :JavaScript, JS: 'AFDate_FormatEx("m/d/yy");' }
  widget_field(doc, form, page, [420, 700, 520, 720], FT: :Tx, T: 'Short Date', AA: { F: format })
  widget_field(doc, form, page, [100, 100, 100, 120], FT: :Tx, T: 'Zero Width')
  widget_field(doc, form, page, [50, 670, 150, 690], FT: :Tx, T: '')
  widget_field(doc, form, page, [170, 670, 270, 690], FT: :Tx, T: 'Duplicate')
  widget_field(doc, form, page, [300, 670, 400, 690], FT: :Tx, T: 'Duplicate')

  person = parent_field(doc, form, T: 'person', FT: :Tx)
  annotate(page, kid(doc, person, page, [50, 640, 150, 660], T: 'first'))
  annotate(page, kid(doc, person, page, [170, 640, 270, 660], T: 'last', Ff: REQUIRED))

  # One field on two pages, listed on the first page before its earlier widget.
  repeated = parent_field(doc, form, T: 'Repeated', FT: :Tx, Q: 1)
  later = kid(doc, repeated, other_page, [50, 600, 250, 620])
  annotate(page, kid(doc, repeated, page, [300, 640, 500, 660]))
  annotate(other_page, later)
end

def add_acro_button_fields(doc, form, page)
  widget_field(doc, form, page, [50, 580, 64, 594], FT: :Btn, T: 'Checked Box', V: :Yes, AS: :Yes,
                                                    AP: on_off(doc, :Yes))
  {
    'Choice Box' => [{}, %i[A B C]],
    'Twin Box' => [{ Ff: REQUIRED }, %i[Yes Yes]],
    'Level' => [{ Ff: RADIO }, %i[0 1 2]],
    'Color' => [{ Ff: RADIO, Opt: %w[Red Green] }, %i[0 1]],
    'Fruit' => [{ Ff: RADIO, Opt: %w[Apple Pear Plum] }, %i[0 1]],
    'Chosen' => [{ Ff: RADIO, V: :b }, %i[a b]],
    'Same State' => [{ Ff: RADIO }, %i[On On]],
    'Opt Box' => [{ Opt: %w[First Second] }, %i[0 1]]
  }.each_with_index do |(name, (dict, states)), row|
    parent = parent_field(doc, form, T: name, FT: :Btn, **dict)
    states.each_with_index do |state, column|
      left = 100 + (column * 30)
      bottom = 550 - (row * 20)
      as = dict[:V] == state ? state : :Off
      annotate(page, kid(doc, parent, page, [left, bottom, left + 14, bottom + 14], AS: as, AP: on_off(doc, state)))
    end
  end
end

def add_acro_choice_fields(doc, form, page)
  widget_field(doc, form, page, [300, 580, 500, 594], FT: :Ch, T: 'Language', Ff: 1 << 17,
                                                      Opt: [%w[de Deutsch], %w[fr Français]])
  widget_field(doc, form, page, [300, 560, 500, 574], FT: :Ch, T: 'Quiet', Ff: 1 << 17, Opt: %w[A B], V: 'B',
                                                      AP: { N: appearance(doc) })
  widget_field(doc, form, page, [300, 540, 500, 554], FT: :Ch, T: 'Pick', Ff: 1 << 17,
                                                      Opt: ['Choose one', 'X'], V: 'Choose one',
                                                      AP: { N: appearance(doc, 'BT /Helv 10 Tf (Choose one) Tj ET') })
  widget_field(doc, form, page, [300, 500, 500, 534], FT: :Ch, T: 'Many', Opt: %w[One Two])
  widget_field(doc, form, page, [300, 480, 500, 494], FT: :Ch, T: 'Empty Combo', Ff: 1 << 17)
end

def add_acro_signature_fields(doc, form, page)
  widget_field(doc, form, page, [50, 300, 150, 340], FT: :Sig, T: 'client_initials')
  widget_field(doc, form, page, [200, 300, 400, 340], FT: :Sig, T: 'Sign Here', TU: 'Sign here please', Ff: REQUIRED)
end

def add_detached_widgets(doc, page)
  widget_field(doc, nil, page, [50, 250, 250, 270], FT: :Tx, T: 'Loose Text', Q: 1, Ff: REQUIRED | COMB, MaxLen: 4)
  widget_field(doc, nil, page, [50, 220, 64, 234], FT: :Btn, T: 'Loose On', V: :On, AS: :On, AP: on_off(doc, :On))
  widget_field(doc, nil, page, [100, 220, 114, 234], FT: :Btn, T: 'Loose Off', V: :Off, AS: :Off,
                                                     AP: on_off(doc, :On))
  widget_field(doc, nil, page, [150, 220, 164, 234], FT: :Btn, T: 'Loose Push', Ff: 1 << 16)
  widget_field(doc, nil, page, [300, 200, 500, 240], FT: :Sig, T: 'Loose Sig')
  widget_field(doc, nil, page, [300, 160, 500, 180], T: 'No Type')
end

# Fields on an offset media box and on pages turned 180 and 270 degrees with a crop box.
def acro_fields_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add([20, 30, 632, 822])
  add_text(doc, page, 'Form fields', 800)
  upside_down = doc.pages.add
  upside_down.rotate(180)
  turned = doc.pages.add
  turned.rotate(270)
  turned[:CropBox] = [50, 50, 500, 700]
  form = doc.acro_form(create: true)
  form[:Fields] ||= []
  form[:DA] = '/Helv 0 Tf 0 g'

  add_acro_text_fields(doc, form, page, upside_down)
  add_acro_button_fields(doc, form, page)
  add_acro_choice_fields(doc, form, page)
  add_acro_signature_fields(doc, form, page)
  add_detached_widgets(doc, page)
  widget_field(doc, form, upside_down, [100, 400, 300, 430], FT: :Tx, T: 'Upside Down')
  widget_field(doc, form, turned, [100, 400, 300, 430], FT: :Tx, T: 'Turned')
  widget_field(doc, form, turned, [100, 300, 114, 314], FT: :Btn, T: 'Turned Box', AP: on_off(doc, :Yes))
  save(doc)
end

# Widgets without an AcroForm: read when the file has a `/Form` (here an XObject), else not.
def orphan_widgets_pdf(with_xobject)
  doc = HexaPDF::Document.new
  page = doc.pages.add
  add_text(doc, page, 'Orphan widgets', 760)
  widget_field(doc, nil, page, [50, 600, 250, 620], FT: :Tx, T: 'Orphan')
  widget_field(doc, nil, page, [50, 560, 64, 574], FT: :Btn, T: 'Orphan Box', AP: on_off(doc, :Yes)) if with_xobject
  save(doc)
end

def link(doc, rect, action)
  doc.add({ Type: :Annot, Subtype: :Link, Rect: rect, A: action })
end

# More than `LINKS_LIMIT` links (the limit is checked per page), other actions and URIs, and
# links on turned and offset pages.
def many_links_pdf
  doc = HexaPDF::Document.new
  first = doc.pages.add
  links = Array.new(240) do |index|
    bottom = 20 + ((index % 60) * 12)
    left = 20 + ((index / 60) * 140)
    link(doc, [left, bottom, left + 120, bottom + 10], { S: :URI, URI: "https://example.com/#{index}" })
  end
  first[:Annots] = links + [
    link(doc, [20, 770, 100, 780], { S: :GoTo, D: [first, :Fit] }),
    link(doc, [120, 770, 200, 780], { S: :Launch, F: 'file.pdf' }),
    link(doc, [220, 770, 300, 780], { S: :URI, URI: 'HTTPS://EXAMPLE.COM/upper' }),
    link(doc, [320, 770, 400, 780], { S: :URI, URI: 'ftp://example.com/file' }),
    link(doc, [420, 770, 500, 780], { S: :URI, URI: ' https://example.com/space' }),
    link(doc, [520, 770, 600, 780], { S: :URI, URI: 'https://example.com/ü?q=a b'.b }),
    doc.add({ Type: :Annot, Subtype: :Link, A: { S: :URI, URI: 'https://example.com/no-rect' } }),
    doc.add({ Type: :Annot, Subtype: :Link, Rect: [20, 750, 100, 760], Dest: [first, :Fit] })
  ]
  second = doc.pages.add([10, 20, 410, 620])
  second.rotate(90)
  second[:Annots] = Array.new(20) do |index|
    link(doc, [20 + (index * 15), 100, 30 + (index * 15), 300], { S: :URI, URI: "http://example.com/second/#{index}" })
  end
  third = doc.pages.add
  third.rotate(270)
  third[:Annots] = [link(doc, [50, 50, 150, 70], { S: :URI, URI: 'https://example.com/past-the-limit' })]
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

def canvas(width, height, color)
  Vips::Image.black(width, height, bands: color.size).new_from_image(color)
end

def color_png
  canvas(800, 600, [255, 255, 255]).draw_rect([220, 30, 30], 100, 100, 200, 200, fill: true)
                                   .draw_rect([30, 30, 30], 400, 300, 300, 20, fill: true)
                                   .write_to_buffer('.png')
end

def gray_png
  canvas(2000, 1333, [255]).draw_rect([40], 100, 100, 800, 800, fill: true).write_to_buffer('.png')
end

def rotated_jpg
  image = canvas(1600, 1000, [255, 255, 255]).draw_rect([30, 60, 200], 100, 100, 600, 300, fill: true)
  image = image.mutate { |m| m.set_type!(GObject::GINT_TYPE, 'orientation', 6) }
  image.write_to_buffer('.jpg', Q: 90)
end

def wide_png
  canvas(1000, 333, [255, 255, 255]).draw_rect([20, 160, 40], 50, 50, 400, 200, fill: true)
                                    .write_to_buffer('.png')
end

# `scripts/golden/acro_fields.rb` loads only the PDFs above.
unless defined?(UPLOADS_SEED_DEFINITIONS_ONLY)
  account = User.order(:id).first.account
  user = account.users.find_or_initialize_by(email: 'parity-uploads@example.com')
  user.assign_attributes(first_name: 'Parity', last_name: 'Uploads', role: 'admin',
                         password: 'parity-uploads-password', archived_at: nil)
  user.save!

  puts JSON.generate(
    email: user.email,
    password: 'parity-uploads-password',
    files: {
      'form.pdf' => form_pdf,
      'links.pdf' => links_pdf,
      'acro_fields.pdf' => acro_fields_pdf,
      'orphan_widgets.pdf' => orphan_widgets_pdf(false),
      'orphan_xobject.pdf' => orphan_widgets_pdf(true),
      'many_links.pdf' => many_links_pdf,
      'pages.pdf' => pages_pdf(18),
      'encrypted.pdf' => encrypted_pdf,
      'color.png' => color_png,
      'gray.png' => gray_png,
      'rotated.jpg' => rotated_jpg,
      'wide.png' => wide_png
    }.transform_values { |data| Base64.strict_encode64(data) }
  )
end
