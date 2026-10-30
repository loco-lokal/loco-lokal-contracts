# Loco Lokal interface theme

The application follows the agency website's current cinematic design, using
`website/contents/landing.ts`, `components/cinematic/BrandMark.vue`, and
`assets/css/cinematic.css` in the sibling `loco-lokal-website` repository as
references.

## Design tokens

| Role | Value |
| --- | --- |
| Primary action | `#ffd700` |
| Hover accent | `#f5b400` |
| Ink | `#14130f` |
| Paper | `#ffffff` |
| Canvas | `#f3f2ee` |
| Secondary surface | `#ebeae5` |
| Muted text | `#5c5a52` |
| Display typography | Anybody, bold condensed italic |
| Body typography | Space Grotesk |

`tailwind.config.js` maps the palette into the existing DaisyUI theme. The
theme name remains `docuseal` so existing layouts and widgets keep working.
`app/javascript/styles/agency.scss` supplies shared branding, navigation,
controls, cards, and authentication/signing styles to both the application
and form bundles. Font files are self-hosted through Webpack.

The wordmark uses `public/loco-lokal-mark.svg`, adapted directly from the
website's vector mark. `public/loco-lokal-icon.png` is the website's favicon.
English and German interface branding is in `config/locales/agency.yml`;
other languages use the configured English fallback.

The visual theme does not change authentication, signing, email delivery,
MCP, PDF generation, or certificate configuration. Document previews retain
their previous default font stack; explicitly selected document fonts and
the isolated dynamic-document stylesheet remain independent of the theme.

DocuSeal attribution remains visible in the application footer and public
signing interface. Product constants, certificate names, and legal notices
are retained.

After changing the theme, compile both frontend bundles and check login,
setup, dashboard/settings, the builder, and public signing at desktop and
mobile widths. In particular, check keyboard focus, disabled/error states,
document field alignment, and the narrow-screen signing header.
