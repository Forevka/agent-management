import type { ReactNode } from 'react'

/*
 * Radio – one of several, each with room to explain itself.
 *
 * affectUI's free set has Checkbox (with a `card` variant), Switch, ToggleGroup and Segmented
 * Control, but no radio, and the fleet needs one: an `AskUserQuestion` from an agent arrives as a
 * few options, each with a description, and the owner picks one. So this is ours, to the same
 * contract, and styled like their card checkbox so the two read as one family.
 *
 * Real `<input type="radio">` elements in a named group, so arrow keys, labels, forms and screen
 * readers all behave without a line of JavaScript.
 */

export type RadioOption = {
  value: string
  label: ReactNode
  description?: ReactNode
  disabled?: boolean
}

export type RadioGroupProps = {
  name: string
  options: RadioOption[]
  value?: string
  onChange?: (value: string) => void
  /** The question these options answer; read out with the group. */
  label?: ReactNode
  className?: string
}

export function RadioGroup({ name, options, value, onChange, label, className }: RadioGroupProps) {
  return (
    <>
      <style href="fx-radio" precedence="default">
        {css}
      </style>
      <fieldset className={['fx-radio', className].filter(Boolean).join(' ')}>
        {label && <legend className="fx-radio__legend">{label}</legend>}
        <div className="fx-radio__list">
          {options.map((o) => (
            <label className="fx-radio__item" key={o.value} data-selected={value === o.value || undefined}>
              <input
                className="fx-radio__input"
                type="radio"
                name={name}
                value={o.value}
                checked={value === o.value}
                disabled={o.disabled}
                onChange={() => onChange?.(o.value)}
              />
              <span className="fx-radio__dot" aria-hidden="true" />
              <span className="fx-radio__text">
                <span className="fx-radio__label">{o.label}</span>
                {o.description && <span className="fx-radio__desc">{o.description}</span>}
              </span>
            </label>
          ))}
        </div>
      </fieldset>
    </>
  )
}

const css = /* css */ `
.fx-radio {
  --fx-rd-line: var(--aui-border, #e5e5e5);
  --fx-rd-line-strong: var(--aui-border-strong, #d4d4d8);
  --fx-rd-ink: var(--aui-ink, #171717);
  --fx-rd-muted: var(--aui-muted, #737373);
  --fx-rd-surface: var(--aui-surface, #ffffff);
  --fx-rd-selected: color-mix(in srgb, var(--aui-ink, #171717) 4%, var(--aui-surface, #fff));

  margin: 0;
  padding: 0;
  border: 0;
  min-width: 0;
}
.fx-radio__legend {
  padding: 0 0 8px;
  font-size: 14px;
  font-weight: 500;
  color: var(--fx-rd-ink);
}
.fx-radio__list {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.fx-radio__item {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 11px 13px;
  background: var(--fx-rd-surface);
  border: 1px solid var(--fx-rd-line);
  border-radius: var(--aui-radius, 10px);
  cursor: pointer;
  transition:
    border-color 140ms ease,
    background-color 140ms ease;
}
.fx-radio__item:hover {
  border-color: var(--fx-rd-line-strong);
}
.fx-radio__item[data-selected] {
  border-color: var(--fx-rd-ink);
  background: var(--fx-rd-selected);
}
.fx-radio__item:has(.fx-radio__input:disabled) {
  opacity: 0.5;
  cursor: default;
}

/* The input stays in the layout for focus and forms, but the dot is what is seen. */
.fx-radio__input {
  position: absolute;
  opacity: 0;
  width: 1px;
  height: 1px;
  margin: 0;
}
.fx-radio__dot {
  display: grid;
  place-items: center;
  flex: none;
  width: 17px;
  height: 17px;
  margin-top: 2px;
  border: 1.5px solid var(--fx-rd-line-strong);
  border-radius: 999px;
  transition:
    border-color 140ms ease,
    box-shadow 140ms ease;
}
.fx-radio__dot::after {
  content: "";
  width: 8px;
  height: 8px;
  border-radius: 999px;
  background: var(--fx-rd-ink);
  transform: scale(0);
  transition: transform 160ms var(--aui-ease, cubic-bezier(0.22, 1, 0.36, 1));
}
.fx-radio__item[data-selected] .fx-radio__dot {
  border-color: var(--fx-rd-ink);
}
.fx-radio__item[data-selected] .fx-radio__dot::after {
  transform: scale(1);
}
.fx-radio__input:focus-visible + .fx-radio__dot {
  outline: 2px solid var(--aui-focus, rgba(0, 96, 226, 0.7));
  outline-offset: 2px;
}

.fx-radio__text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.fx-radio__label {
  font-size: 13.5px;
  font-weight: 500;
  color: var(--fx-rd-ink);
  overflow-wrap: anywhere;
}
.fx-radio__desc {
  font-size: 12.5px;
  color: var(--fx-rd-muted);
  overflow-wrap: anywhere;
}

:where(.dark, [data-theme="dark"]) .fx-radio {
  --fx-rd-line: var(--aui-border, #2a2a2e);
  --fx-rd-line-strong: var(--aui-border-strong, #3a3a40);
  --fx-rd-ink: var(--aui-ink, #ededed);
  --fx-rd-muted: var(--aui-muted, #8b8b92);
  --fx-rd-surface: var(--aui-surface, #19191b);
  --fx-rd-selected: rgba(255, 255, 255, 0.06);
}
`
