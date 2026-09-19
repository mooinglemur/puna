// Sections that remember having been folded away, and the one inverted polarity holding them up.
//
// **The store's key names the COLLAPSED state, not the open one.** `set` deletes rather than
// storing `false`, so an absent entry already means "not turned on"; naming the collapsed state is
// what makes absent mean OPEN, which is the default every section on the tracker is specified
// against. Store the open state instead and the whole thing works perfectly in reverse: a reader
// who folds a section finds it open next visit and one who leaves it open finds it folded. Nothing
// errors, nothing logs, and it would reasonably be reported as the preference not saving at all.
//
// A source lint pins the two lines that hold that polarity; this runs them, which is the only thing
// that can tell the two directions apart. `toggles.js` is an IIFE over `document` and
// `localStorage`, so the whole file is run against stubs for both rather than a block being lifted
// out of it: what is under test is the round trip, and half of that lives in `get`/`set`.
"use strict";

const fs = require("fs");
const path = require("path");
const vm = require("vm");

const source = path.join(__dirname, "..", "..", "static", "toggles.js");

// A `<details>` as this file uses one: an open flag, a dataset, and somewhere for the listener to
// go. `toggle` fires AFTER the browser has moved `open`, which is why nothing here keeps a second
// copy of the state, so `fold()` sets it before calling back.
function details(key) {
  const node = {
    open: true,
    dataset: { collapsed: key },
    listeners: {},
    addEventListener(name, fn) {
      node.listeners[name] = fn;
    },
  };
  node.fold = () => {
    node.open = false;
    node.listeners.toggle();
  };
  node.unfold = () => {
    node.open = true;
    node.listeners.toggle();
  };
  return node;
}

// A store that starts however a previous visit left it, plus a window and a document with nothing
// in them: `bind(document)` runs on load and must find no controls rather than throwing.
function harness(stored) {
  let raw = stored === undefined ? null : JSON.stringify(stored);
  const nodes = [];
  const context = {
    window: {
      localStorage: {
        getItem: () => raw,
        setItem: (_key, value) => {
          raw = value;
        },
      },
    },
    document: { querySelectorAll: () => nodes },
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(source, "utf8"), context, { filename: "toggles.js" });
  return {
    toggles: context.window.PunaToggles,
    // Bound one at a time, so each test says which elements exist on its page.
    bind: (node) => {
      nodes.length = 0;
      nodes.push(node);
      context.window.PunaToggles.bind(context.document);
      nodes.length = 0;
      return node;
    },
    stored: () => JSON.parse(raw || "{}"),
  };
}

const KEY = "tracker.room.hints.collapsed";

exports.run = function (t) {
  // --- a reader who has never expressed a preference --------------------------------------------
  //
  // The markup ships `open`; an empty store must leave it that way. Getting this wrong greets a
  // first-time reader with every section folded shut, which reads as content that failed to load.
  {
    const h = harness({});
    const node = h.bind(details(KEY));
    t.check("an untouched section stays open", node.open === true);
    t.check("and nothing is written for it", Object.keys(h.stored()).length === 0);
  }

  // --- folding one is remembered ----------------------------------------------------------------
  {
    const h = harness({});
    const node = h.bind(details(KEY));
    node.fold();
    t.check("folding it stores the key", h.stored()[KEY] === true);

    // And the next visit honors it.
    const next = harness(h.stored());
    const restored = next.bind(details(KEY));
    t.check("so the next visit arrives folded", restored.open === false);
  }

  // --- and unfolding it again forgets, rather than storing a false ------------------------------
  //
  // The store holds only what somebody turned on, which is the property that lets absent mean open.
  {
    const h = harness({ [KEY]: true });
    const node = h.bind(details(KEY));
    t.check("it is restored folded", node.open === false);
    node.unfold();
    t.check("unfolding it clears the entry", h.stored()[KEY] === undefined);
    t.check("rather than storing a false", !(KEY in h.stored()));
  }

  // --- the polarity, stated as the round trip it has to be --------------------------------------
  //
  // Both directions, because an implementation that stored the OPEN state passes any single-sided
  // assertion by reading its own writes back.
  {
    const h = harness({});
    const folded = h.bind(details(KEY));
    folded.fold();
    const after = harness(h.stored()).bind(details(KEY));
    t.check("folded stays folded across a visit", after.open === false);

    const g = harness({});
    const left = g.bind(details(KEY));
    left.unfold();
    const also = harness(g.stored()).bind(details(KEY));
    t.check("and open stays open", also.open === true);
  }

  // --- one key per section ----------------------------------------------------------------------
  //
  // The keys are a flat namespace shared with every checkbox on every page, which is why they are
  // spelled out by hand in the markup.
  {
    const h = harness({});
    const hints = h.bind(details(KEY));
    hints.fold();
    const slots = harness(h.stored()).bind(details("tracker.room.slots.collapsed"));
    t.check("folding one section leaves the others alone", slots.open === true);
  }

  // --- binding twice does not double up ---------------------------------------------------------
  //
  // `bind` is exposed for controls that arrive after load, so it runs over elements it has already
  // seen. Two listeners on one element would write the store twice per click.
  {
    const h = harness({});
    const node = details(KEY);
    h.bind(node);
    let bound = 0;
    const record = node.addEventListener;
    node.addEventListener = (name, fn) => {
      bound++;
      record(name, fn);
    };
    h.bind(node);
    t.check("a second bind adds no second listener", bound === 0);
  }

  // --- an unstorable preference still works for the life of the page ----------------------------
  //
  // `localStorage` THROWS rather than returning null in a private window and wherever storage is
  // blocked for an origin. A section that cannot be remembered must still fold.
  {
    const nodes = [];
    const context = {
      window: {
        localStorage: {
          getItem: () => {
            throw new Error("blocked");
          },
          setItem: () => {
            throw new Error("blocked");
          },
        },
      },
      document: { querySelectorAll: () => nodes },
    };
    vm.createContext(context);
    vm.runInContext(fs.readFileSync(source, "utf8"), context, { filename: "toggles.js" });
    const node = details(KEY);
    nodes.push(node);
    let threw = false;
    try {
      context.window.PunaToggles.bind(context.document);
      node.fold();
    } catch (e) {
      threw = true;
    }
    t.check("blocked storage neither throws on restore nor on write", !threw);
    t.check("and the section is still folded on the page it was folded on", node.open === false);
  }
};
