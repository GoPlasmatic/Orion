#!/usr/bin/env node
// Drives the Orion console (Orion-ui) with Playwright to produce the README
// hero recording and the static screenshots. Invoked by ../record-ui.sh,
// which boots a throwaway orion-server and the Orion-ui dev server first.
//
//   demo-ui-quickstart.mjs record --theme light --out out/light \
//       --base http://localhost:5273 --orion http://localhost:8080 \
//       --examples ../../examples/high-value-order
//   demo-ui-quickstart.mjs stills ...same flags...
//
// `record` captures the creation loop as out/<theme>/record.webm:
//   import workflow (paste -> validate -> dry-run -> activate) -> DAG ->
//   create channel form -> activate -> Data Console send -> System Map.
// `stills` fires a traffic burst at the channel, then screenshots
// Operations, System Map, workflow DAG, and the Data Console.

import { mkdirSync, readFileSync } from "node:fs"
import { join } from "node:path"
import { chromium } from "playwright"

const argv = process.argv.slice(2)
const mode = argv[0]
const flag = (name, dflt) => {
  const i = argv.indexOf(`--${name}`)
  return i > -1 && argv[i + 1] ? argv[i + 1] : dflt
}
if (mode !== "record" && mode !== "stills") {
  console.error("usage: demo-ui-quickstart.mjs <record|stills> [--theme light|dark] [--out dir] [--base url] [--orion url] [--examples dir]")
  process.exit(2)
}
const theme = flag("theme", "light")
const out = flag("out", `out/${theme}`)
const base = flag("base", "http://localhost:5273")
const orion = flag("orion", "http://localhost:8080")
const examples = flag("examples", "../../examples/high-value-order")

mkdirSync(out, { recursive: true })

const workflowJson = readFileSync(join(examples, "workflow.json"), "utf8").trim()
const request = JSON.parse(readFileSync(join(examples, "request.json"), "utf8"))
const orderPayload = JSON.stringify(request.data, null, 2)

const pause = (ms) => new Promise((r) => setTimeout(r, ms))

// Fake cursor + caption bar, injected on every navigation. Playwright's video
// capture has no OS cursor, so a dot follows the synthetic mousemove events.
const OVERLAY = `(() => {
  const ensure = () => {
    if (!document.body || document.getElementById("__demo-cursor")) return
    const cur = document.createElement("div")
    cur.id = "__demo-cursor"
    Object.assign(cur.style, {
      position: "fixed", left: "-40px", top: "-40px", width: "18px", height: "18px",
      borderRadius: "50%", background: "rgba(30,30,30,.55)",
      border: "2px solid rgba(255,255,255,.92)", boxShadow: "0 1px 4px rgba(0,0,0,.45)",
      zIndex: 2147483647, pointerEvents: "none", transform: "translate(-50%,-50%)",
      transition: "width .12s, height .12s",
    })
    document.body.appendChild(cur)
    const cap = document.createElement("div")
    cap.id = "__demo-caption"
    Object.assign(cap.style, {
      position: "fixed", bottom: "20px", left: "50%", transform: "translateX(-50%)",
      maxWidth: "85%", background: "rgba(17,24,39,.9)", color: "#fff",
      padding: "10px 22px", borderRadius: "999px", whiteSpace: "nowrap",
      font: "500 15px/1.4 -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
      zIndex: 2147483647, pointerEvents: "none", boxShadow: "0 4px 16px rgba(0,0,0,.35)",
      opacity: "0", transition: "opacity .35s",
    })
    document.body.appendChild(cap)
    document.addEventListener("mousemove", (e) => {
      cur.style.left = e.clientX + "px"
      cur.style.top = e.clientY + "px"
    }, true)
    document.addEventListener("mousedown", () => { cur.style.width = "26px"; cur.style.height = "26px" }, true)
    document.addEventListener("mouseup", () => { cur.style.width = "18px"; cur.style.height = "18px" }, true)
  }
  window.__caption = (text) => {
    ensure()
    const c = document.getElementById("__demo-caption")
    if (!c) return
    if (!text) { c.style.opacity = "0"; return }
    c.textContent = text
    c.style.opacity = "1"
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", ensure)
  else ensure()
})()`

const caption = (page, text) => page.evaluate((t) => window.__caption?.(t), text)

async function move(page, locator) {
  await locator.scrollIntoViewIfNeeded()
  const box = await locator.boundingBox()
  if (!box) throw new Error(`no bounding box for ${locator}`)
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 25 })
  await pause(200)
}

async function click(page, locator) {
  await move(page, locator)
  await page.mouse.down()
  await pause(90)
  await page.mouse.up()
  await pause(350)
}

// The form labels are not wired to their controls with htmlFor, so target the
// control adjacent to the label text.
const byLabel = (page, label, control = "input") =>
  page.locator(`xpath=//label[normalize-space()="${label}"]/following-sibling::${control}[1]`)

async function typeInto(page, locator, text) {
  await click(page, locator)
  await locator.pressSequentially(text, { delay: 26 })
  await pause(250)
}

async function pasteInto(page, locator, text) {
  await click(page, locator)
  await locator.fill(text)
  await pause(300)
}

async function sendFromConsole(page) {
  const channelSelect = byLabel(page, "Channel", "select")
  await click(page, channelSelect)
  await channelSelect.selectOption("orders")
  await pause(400)
  await pasteInto(page, page.locator("textarea"), orderPayload)
  await click(page, page.getByRole("checkbox", { name: "Profile" }))
  await click(page, page.getByRole("button", { name: "Send", exact: true }))
  await page.getByRole("link", { name: "Open as trace" }).waitFor({ timeout: 15000 })
}

async function record() {
  const browser = await chromium.launch()
  const context = await browser.newContext({
    viewport: { width: 1280, height: 800 },
    colorScheme: theme,
    recordVideo: { dir: out, size: { width: 1280, height: 800 } },
  })
  globalThis.__lastContext = context
  await context.addInitScript((t) => localStorage.setItem("orion-theme", t), theme)
  await context.addInitScript(OVERLAY)
  const page = await context.newPage()

  // Scene 0 — a running Orion, empty workflows list.
  await page.goto(`${base}/workflows`)
  await page.getByText("No workflows yet").waitFor({ timeout: 20000 })
  await pause(700)
  await caption(page, "Orion is running. Let's ship a service — no code.")
  await pause(2300)

  // Scene 1 — import wizard: paste -> validate -> import -> dry-run -> activate.
  await caption(page, "Declare the logic. Validate and test it before it goes live.")
  await click(page, page.getByRole("button", { name: "Import workflow" }))
  const dialog = page.getByRole("dialog")
  await dialog.waitFor()
  await pasteInto(page, dialog.locator("textarea"), workflowJson)
  await pause(1000)
  await click(page, dialog.getByRole("button", { name: "Validate" }))
  await dialog.getByText("Definition is valid").waitFor()
  await pause(1800)
  await click(page, dialog.getByRole("button", { name: "Next" }))
  await pause(700)
  await click(page, dialog.getByRole("button", { name: "Import as draft" }))
  // On success the wizard advances to the dry-run step by itself.
  await dialog.getByText("Dry-run the imported draft").waitFor()
  await pause(1100)
  await pasteInto(page, dialog.locator("textarea"), orderPayload)
  await pause(600)
  await click(page, dialog.getByRole("button", { name: "Run dry-run" }))
  await dialog.getByText("Matched").waitFor()
  await pause(2100)
  await click(page, dialog.getByRole("button", { name: "Next" }))
  await click(page, dialog.getByRole("button", { name: "Activate workflow" }))
  await dialog.getByText("Workflow activated.").waitFor()
  await pause(1500)
  await click(page, dialog.getByRole("link", { name: "View workflows" }))

  // Scene 2 — the imported pipeline rendered as a DAG. The visualizer shows an
  // empty state until an explorer item is selected.
  await caption(page, "Your service logic, visualized.")
  await click(page, page.getByRole("cell", { name: "High-Value Order" }))
  await page.waitForURL("**/workflows/high-value-order")
  await pause(1300)
  await click(page, page.getByText("Flag order"))
  await pause(2800)

  // Scene 3 — give it an endpoint: the channel form.
  await caption(page, "Give it an endpoint — a form, not a framework.")
  await click(page, page.getByRole("link", { name: "Channels" }))
  await click(page, page.getByRole("button", { name: "Create Channel" }).first())
  await page.waitForURL("**/channels/new")
  await typeInto(page, byLabel(page, "Name"), "orders")
  await typeInto(page, byLabel(page, "Description"), "Order intake endpoint")
  const protocol = byLabel(page, "Protocol", "select")
  await click(page, protocol)
  await protocol.selectOption("http")
  await typeInto(page, byLabel(page, "Methods"), "POST")
  await typeInto(page, byLabel(page, "Route Pattern"), "/orders")
  await typeInto(page, byLabel(page, "Linked Workflow ID"), "high-value-order")
  await click(page, page.getByRole("button", { name: "Save" }))
  // The server assigns the channel_id; the form redirects to the detail page.
  await page.getByRole("button", { name: "Activate" }).waitFor()
  await pause(900)
  await click(page, page.getByRole("button", { name: "Activate" }))
  await page.getByRole("button", { name: "Activate" }).waitFor({ state: "detached" })
  await pause(1400)

  // Scene 4 — it's live: send a request from the Data Console.
  await caption(page, "Live — with tracing and per-task timings built in.")
  await click(page, page.getByRole("link", { name: "Data Console" }))
  await sendFromConsole(page)
  await pause(2800)

  // Scene 5 — the service on the System Map.
  await caption(page, "A governed service. No code. No redeploy.")
  await click(page, page.getByRole("link", { name: "System Map" }))
  await pause(4200)

  const video = page.video()
  await context.close()
  await video.saveAs(join(out, "record.webm"))
  await browser.close()
  console.log(`record -> ${join(out, "record.webm")}`)
}

// Fire varied traffic at the channel so dashboards/traces have data to show.
async function burst(n = 70) {
  for (let i = 0; i < n; i++) {
    const total = Math.round(200 + Math.random() * 29800)
    await fetch(`${orion}/api/v1/data/orders`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ data: { order_id: `ORD-${1000 + i}`, total } }),
    }).catch(() => {})
  }
}

async function stills() {
  await burst()
  const browser = await chromium.launch()
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 2,
    colorScheme: theme,
  })
  globalThis.__lastContext = context
  await context.addInitScript((t) => localStorage.setItem("orion-theme", t), theme)
  const page = await context.newPage()

  const shoot = async (name) => {
    const path = join(out, `${name}.png`)
    await page.screenshot({ path })
    console.log(`still -> ${path}`)
  }

  // The request-rate chart needs two /metrics samples (10s poll) with traffic
  // in between; totals and outcomes render from the first sample.
  await page.goto(`${base}/`)
  await pause(1500)
  await burst(40)
  await pause(11000)
  await shoot("operations")

  await page.goto(`${base}/system-map`)
  await pause(2500)
  await shoot("system-map")

  await page.goto(`${base}/workflows/high-value-order`)
  await pause(1800)
  await page.getByText("Flag order").click()
  await pause(1500)
  await shoot("workflow-dag")

  await page.goto(`${base}/console`)
  await pause(1200)
  await sendFromConsole(page)
  await pause(800)
  await shoot("console")

  await context.close()
  await browser.close()
}

try {
  await (mode === "record" ? record() : stills())
} catch (err) {
  // Leave evidence of where the run died; the video (if any) is in `out` too.
  for (const ctx of globalThis.__lastContext ? [globalThis.__lastContext] : []) {
    await ctx.pages()[0]?.screenshot({ path: join(out, "failure.png") }).catch(() => {})
  }
  throw err
}
