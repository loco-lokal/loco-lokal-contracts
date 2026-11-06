# frozen_string_literal: true

module Api
  class ActiveStorageBlobsProxyLegacyController < ApiBaseController
    include ActiveStorage::Streaming

    skip_before_action :authenticate_user!
    skip_authorization_check

    before_action :set_cors_headers
    before_action :set_noindex_headers
    before_action :set_security_headers

    # rubocop:disable Metrics
    def show
      blob = ActiveStorage::Blob.find_signed(params[:signed_blob_id] || params[:signed_id])

      return head :not_found unless blob

      return head :unprocessable_content if FilenameUtils.dangerous_extension(blob.filename)

      is_permitted = blob.attachments.any? do |a|
        (current_user && a.record.account.id == current_user.account_id) ||
          a.record.account.account_configs.any? { |e| e.key == 'legacy_blob_proxy' } ||
          a.name == 'logo'
      end

      return render json: { error: 'Not authenticated' }, status: :unauthorized unless is_permitted

      if request.headers['Range'].present?
        send_blob_byte_range_data blob, request.headers['Range']
      else
        http_cache_forever public: true do
          response.headers['Accept-Ranges'] = 'bytes'
          response.headers['Content-Length'] = blob.byte_size.to_s

          send_blob_stream blob, disposition: params[:disposition]
        end
      end
    end
    # rubocop:enable Metrics
  end
end
