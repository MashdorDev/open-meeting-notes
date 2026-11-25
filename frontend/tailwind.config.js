/** @type {import('tailwindcss').Config} */
const plugin = require('tailwindcss/plugin')
const twColors = require('tailwindcss/colors')

// Dark theme without touching every class: the gray, white and accent
// colors used across the app read from CSS variables, and `.dark` swaps
// those variables. Gray-like scales share one hand-picked dark scale where
// white is the card surface, gray-50 the page behind it, and higher shades
// get lighter so text stays readable. Accent scales mirror (50 <-> 950,
// 100 <-> 900, ...), so pale tints turn into deep ones and dark text turns
// light. Black is left alone: it is only used for modal backdrops.
const SHADES = [50, 100, 200, 300, 400, 500, 600, 700, 800, 900, 950]
const GRAY_SCALES = ['gray', 'slate', 'zinc', 'neutral', 'stone']
const ACCENT_SCALES = ['red', 'orange', 'amber', 'yellow', 'green', 'emerald', 'blue', 'purple']
const DARK_GRAY = {
  50: '#0c0c0e', 100: '#27272a', 200: '#3f3f46', 300: '#52525b', 400: '#71717a',
  500: '#a1a1aa', 600: '#c4c4cc', 700: '#d4d4d8', 800: '#e4e4e7', 900: '#f4f4f5', 950: '#fafafa',
}
const DARK_WHITE = '#18181b'

const rgb = (hex) => {
  const n = parseInt(hex.slice(1), 16)
  return `${(n >> 16) & 255} ${(n >> 8) & 255} ${n & 255}`
}
const varColor = (name) => `rgb(var(--tw-${name}) / <alpha-value>)`

const themedColors = { white: varColor('white') }
const lightVars = { '--tw-white': rgb('#ffffff') }
const darkVars = { '--tw-white': rgb(DARK_WHITE) }
for (const scale of [...GRAY_SCALES, ...ACCENT_SCALES]) {
  themedColors[scale] = {}
  SHADES.forEach((shade, i) => {
    const name = `${scale}-${shade}`
    themedColors[scale][shade] = varColor(name)
    lightVars[`--tw-${name}`] = rgb(twColors[scale][shade])
    darkVars[`--tw-${name}`] = rgb(
      GRAY_SCALES.includes(scale) ? DARK_GRAY[shade] : twColors[scale][SHADES[SHADES.length - 1 - i]]
    )
  })
}

const themedPalette = plugin(({ addBase }) => {
  addBase({ ':root': { ...lightVars, colorScheme: 'light' }, '.dark': { ...darkVars, colorScheme: 'dark' } })
})

module.exports = {
    darkMode: ['class'],
    content: [
    './src/pages/**/*.{js,ts,jsx,tsx,mdx}',
    './src/components/**/*.{js,ts,jsx,tsx,mdx}',
    './src/app/**/*.{js,ts,jsx,tsx,mdx}',
  ],
  theme: {
  	extend: {
  		fontFamily: {
  			sans: [
  				'var(--font-figtree)'
  			],
  			heading: [
  				'var(--font-bricolage)'
  			]
  		},
  		colors: {
  			...themedColors,
  			background: 'hsl(var(--background))',
  			foreground: 'hsl(var(--foreground))',
  			border: 'hsl(var(--border))',
  			input: 'hsl(var(--input))',
  			ring: 'hsl(var(--ring))',
  			primary: {
  				DEFAULT: 'hsl(var(--primary))',
  				foreground: 'hsl(var(--primary-foreground))'
  			},
  			secondary: {
  				DEFAULT: 'hsl(var(--secondary))',
  				foreground: 'hsl(var(--secondary-foreground))'
  			},
  			tertiary: '#64748b',
  			card: {
  				DEFAULT: 'hsl(var(--card))',
  				foreground: 'hsl(var(--card-foreground))'
  			},
  			popover: {
  				DEFAULT: 'hsl(var(--popover))',
  				foreground: 'hsl(var(--popover-foreground))'
  			},
  			muted: {
  				DEFAULT: 'hsl(var(--muted))',
  				foreground: 'hsl(var(--muted-foreground))'
  			},
  			accent: {
  				DEFAULT: 'hsl(var(--accent))',
  				foreground: 'hsl(var(--accent-foreground))'
  			},
  			destructive: {
  				DEFAULT: 'hsl(var(--destructive))',
  				foreground: 'hsl(var(--destructive-foreground))'
  			},
  			chart: {
  				'1': 'hsl(var(--chart-1))',
  				'2': 'hsl(var(--chart-2))',
  				'3': 'hsl(var(--chart-3))',
  				'4': 'hsl(var(--chart-4))',
  				'5': 'hsl(var(--chart-5))'
  			}
  		},
  		borderRadius: {
  			lg: 'var(--radius)',
  			md: 'calc(var(--radius) - 2px)',
  			sm: 'calc(var(--radius) - 4px)'
  		},
  		keyframes: {
  			'accordion-down': {
  				from: {
  					height: '0'
  				},
  				to: {
  					height: 'var(--radix-accordion-content-height)'
  				}
  			},
  			'accordion-up': {
  				from: {
  					height: 'var(--radix-accordion-content-height)'
  				},
  				to: {
  					height: '0'
  				}
  			}
  		},
  		animation: {
  			'accordion-down': 'accordion-down 0.2s ease-out',
  			'accordion-up': 'accordion-up 0.2s ease-out'
  		}
  	}
  },
  plugins: [
    require("tailwindcss-animate"),
    require("@tailwindcss/typography"),
    require("@tailwindcss/container-queries"),
    themedPalette,
  ],
}