module.exports = {
  future: {
    hoverOnlyWhenSupported: true
  },
  theme: {
    extend: {
      fontFamily: {
        display: ['Anybody', 'Impact', 'sans-serif']
      }
    }
  },
  plugins: [
    require('daisyui')
  ],
  daisyui: {
    themes: [
      {
        docuseal: {
          'color-scheme': 'light',
          primary: '#ffd700',
          'primary-content': '#14130f',
          secondary: '#f6f1e2',
          'secondary-content': '#14130f',
          accent: '#f5b400',
          'accent-content': '#14130f',
          neutral: '#14130f',
          'neutral-content': '#ffffff',
          'base-100': '#ffffff',
          'base-200': '#f3f2ee',
          'base-300': '#ebeae5',
          'base-content': '#14130f',
          '--rounded-box': '1.5rem',
          '--rounded-btn': '999px',
          '--rounded-badge': '999px',
          '--tab-border': '2px',
          '--tab-radius': '.5rem'
        }
      }
    ]
  }
}
