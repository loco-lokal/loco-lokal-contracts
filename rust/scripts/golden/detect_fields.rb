# frozen_string_literal: true

# Golden output of `Templates::DetectFields` for crates/detect/tests/golden.rs: the
# fields of each page of detect-form.pdf, and of the image previews detect-image.png
# and detect-tall.png, as the controller calls it (field uuids left out). The fixtures
# are copied to /tmp by generate-golden.sh.

ImageDocument = Struct.new(:uuid) do
  def image?
    true
  end
end

def detect(path, attachment)
  pages = []
  Templates::DetectFields.call(StringIO.new(File.binread(path)), attachment:) do |(_, page, fields)|
    pages << { page:, fields: fields.map { |f| f.except(:uuid) } }
  end
  pages
end

puts ActiveSupport::JSON.encode(
  'detect-form.pdf' => detect('/tmp/detect-form.pdf', nil),
  'detect-image.png' => detect('/tmp/detect-image.png', ImageDocument.new(nil)),
  'detect-tall.png' => detect('/tmp/detect-tall.png', ImageDocument.new(nil))
)
