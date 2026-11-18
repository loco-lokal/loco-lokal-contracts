# frozen_string_literal: true

# Golden data for `crates/pdf/tests/upload.rs` (run by `scripts/generate-golden.sh`): the
# PDFs of `scripts/parity/uploads_seed.rb` (given as the argument) with form fields and
# links, and what `Templates::FindPdfiumAcroFields` and `Templates::BuildPdfiumAnnotations`
# return for them, with "doc" as the attachment uuid. Prints one JSON object:
# `{ name => { "pdf": base64, "fields": json, "annotations": json } }`. The results are JSON
# text as Rails stores it (`to_json`, whose floats are not always the shortest digits), with
# uuids numbered in order of appearance.

UPLOADS_SEED_DEFINITIONS_ONLY = true
load ARGV.fetch(0)

attachment = Struct.new(:uuid).new('doc')

UUID_REGEXP = /\A\h{8}-\h{4}-\h{4}-\h{4}-\h{12}\z/

def number_uuids(value, numbers = {})
  case value
  when Hash then value.to_h { |k, v| [k, number_uuids(v, numbers)] }
  when Array then value.map { |v| number_uuids(v, numbers) }
  when UUID_REGEXP then "uuid #{numbers[value] ||= numbers.size}"
  else value
  end
end

files = {
  'upload-form.pdf' => form_pdf,
  'upload-acro-fields.pdf' => acro_fields_pdf,
  'upload-orphan-widgets.pdf' => orphan_widgets_pdf(false),
  'upload-orphan-xobject.pdf' => orphan_widgets_pdf(true),
  'upload-many-links.pdf' => many_links_pdf
}

puts JSON.generate(
  files.to_h do |name, data|
    doc = Pdfium::Document.open_bytes(data)
    result = {
      pdf: Base64.strict_encode64(data),
      fields: number_uuids(Templates::FindPdfiumAcroFields.call(attachment, doc, data)).to_json,
      annotations: number_uuids(Templates::BuildPdfiumAnnotations.call(doc)).to_json
    }
    doc.close
    [name, result]
  end
)
