"use client"

import { useState } from "react"
import { Monitor, Moon, Sun } from "lucide-react"
import { loadThemeChoice, saveThemeChoice, ThemeChoice } from "@/lib/theme"

const OPTIONS: { value: ThemeChoice; label: string; icon: typeof Sun }[] = [
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
  { value: "system", label: "System", icon: Monitor },
]

export function ThemeSettings() {
  const [choice, setChoice] = useState<ThemeChoice>(loadThemeChoice)

  const choose = (value: ThemeChoice) => {
    setChoice(value)
    saveThemeChoice(value)
  }

  return (
    <div className="bg-white rounded-lg border border-gray-200 p-6 shadow-sm">
      <div className="flex items-center justify-between gap-6">
        <div>
          <h3 className="text-lg font-semibold text-gray-900 mb-2">Theme</h3>
          <p className="text-sm text-gray-600">System follows your computer&apos;s light or dark setting</p>
        </div>
        <div role="radiogroup" aria-label="Theme" className="flex shrink-0 rounded-lg border border-gray-200 bg-gray-50 p-1">
          {OPTIONS.map(({ value, label, icon: Icon }) => (
            <button
              key={value}
              type="button"
              role="radio"
              aria-checked={choice === value}
              onClick={() => choose(value)}
              className={`flex items-center gap-2 rounded-md px-3 py-1.5 text-sm transition-colors ${
                choice === value
                  ? "bg-white text-gray-900 shadow-sm"
                  : "text-gray-600 hover:text-gray-900"
              }`}
            >
              <Icon className="h-4 w-4" />
              {label}
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}
