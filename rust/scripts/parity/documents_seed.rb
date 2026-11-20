# frozen_string_literal: true

# Prepares documents.py (run through `bin/rails runner` after `uploads_seed.rb`'s
# definitions): the uploads user, and the files of uploads_seed.rb plus PDFs and images
# for the page editor's branches. Prints one JSON object like uploads_seed.rb.

def image_text_pdf
  doc = HexaPDF::Document.new
  page = doc.pages.add
  canvas = page.canvas
  canvas.font('Helvetica', size: 14).text('Name: Jane Example', at: [50, 760])
  canvas.font('Helvetica', size: 14).text('Account 12345678', at: [50, 730])
  canvas.font('Helvetica', size: 14).text('Keep this line', at: [50, 700])
  picture = Vips::Image.black(120, 80, bands: 3).new_from_image([30, 120, 220])
                       .draw_rect([250, 200, 20], 20, 20, 40, 30, fill: true)
  canvas.image(StringIO.new(picture.write_to_buffer('.png')), at: [50, 450], width: 240)

  form = doc.add({ Type: :XObject, Subtype: :Form, BBox: [0, 0, 300, 60] })
  form.canvas.font('Helvetica', size: 12).text('Inside a form XObject', at: [10, 20])
  canvas.xobject(form, at: [50, 300])

  second = doc.pages.add
  second.canvas.font('Helvetica', size: 14).text('Second page', at: [50, 760])
  save(doc)
end

def letter_scan_png
  canvas(1275, 1650, [255, 255, 255]).draw_rect([20, 20, 20], 100, 100, 900, 30, fill: true)
                                      .draw_rect([20, 20, 20], 100, 200, 700, 30, fill: true)
                                      .write_to_buffer('.png')
end

def photo_png
  Vips::Image.black(900, 700, bands: 3).new_from_image([40, 90, 160])
             .draw_rect([200, 60, 60], 100, 100, 300, 300, fill: true)
             .draw_rect([240, 220, 40], 500, 300, 300, 300, fill: true)
             .write_to_buffer('.png')
end

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
    'pages.pdf' => pages_pdf(4),
    'encrypted.pdf' => encrypted_pdf,
    'image_text.pdf' => image_text_pdf,
    'color.png' => color_png,
    'gray.png' => gray_png,
    'rotated.jpg' => rotated_jpg,
    'letter_scan.png' => letter_scan_png,
    'photo.png' => photo_png
  }.transform_values { |data| Base64.strict_encode64(data) }
)
