# frozen_string_literal: true

# Dumps `TextUtils.maybe_rtl_reverse` for sample texts (Hebrew, Arabic, numbers,
# punctuation, mixed directions, explicit embeddings and overrides) as JSON pairs, so the
# Rust port of TwitterCldr's Bidi and ArabicLetterConnector is checked against Rails.

samples = [
  'hello', 'שלום', 'שלום world', 'שלום 123', 'שלום, עולם!', 'מספר 12.5% ו-3,000',
  'דוד כהן <david@example.com>', 'חתום על ידי: דוד כהן', 'קובץ.pdf', 'סעיף (א) [ב] {ג}',
  'בית', 'دار', 'مرحبا بالعالم', 'السلام عليكم', 'رقم 123 و ٤٥٦', 'محمد أحمد', 'لا إله',
  'فاتورة رقم: 2026-11-12', 'abc مرحبا def', 'Name: אבי Cohen', 'תאריך 12/11/2026 10:30',
  "שורה\nשנייה", "a\tב", 'עִבְרִית', 'كَتَبَ', 'ـــ', 'هـ', '+972 50-123-4567', '1 - שלום - 2',
  "‫שלום abc‬", "‮abc שלום‬", "‪שלום‬ abc", "‭אבג‬", 'א ‎b',
  "€ 100 שקל", '#1 בדיקה', 'ספר "הבית"', 'x = 3 + 4 × 2 بالدقيقة'
]

puts samples.map { |s| [s, TextUtils.maybe_rtl_reverse(s)] }.to_json
