//! A lead's orders in its ElevaLeads brand, and the form that puts a sale
//! closed on WhatsApp there.
//!
//! The card reads, by phone, what the brand's customer bought; a customer
//! here with no order there is a sale the shop does not know about, and the
//! card says so with a way to register it. The form says, before sending,
//! whether the brand's automations would message the customer for the
//! status picked; closed and paid is the default, which usually sends
//! nothing.
//!
//! The threads write what they learn straight into the leads store, so the
//! panel's count of unregistered sales stays right without the card open.

use std::time::Duration;

use egui::{Align, Frame, Layout, Margin, vec2};

use super::leads::{button, card_frame, chip, error_text, field_frame, notice_text, wrapped};
use crate::app::App;
use crate::eleva::sales::{self, Answer, Automations, NewSale, PAYMENTS, STATUSES};
use crate::i18n::{Locale, gettext};
use crate::leads::{Brand, ElevaCustomer, Kind, Lead, Leads, OrdersSeen, Sale, Store, Sync};
use crate::projects::Project;
use crate::theme::{self, Icon};
use crate::ui::widgets;

/// Orders are read again after this long.
const STALE: i64 = 6 * 3600;

/// The sale being written for a lead.
#[derive(Clone, Default)]
struct Form {
    open: bool,
    /// Description, quantity and unit price, as typed.
    items: Vec<(String, String, String)>,
    total: String,
    total_typed: bool,
    status: usize,
    payment: usize,
    notes: String,
    /// The id it goes with, kept across retries.
    id: String,
    key: String,
    error: Option<String>,
    /// Whether the brand's API key is in the keyring.
    has_key: Option<bool>,
    /// Register even though the brand's automatic messages could not be
    /// read.
    unknown_ok: bool,
}

fn form_id(lead: &str) -> egui::Id {
    egui::Id::new(("sale-form", lead))
}

fn status_label(locale: Locale, status: &str) -> String {
    match status {
        "cerrado" => gettext(locale, "Closed"),
        "confirmado" => gettext(locale, "Confirmed"),
        "pago" => gettext(locale, "Payment in"),
        "envio" => gettext(locale, "On its way"),
        "recepcion" => gettext(locale, "Delivered"),
        "pagado" => gettext(locale, "Paid"),
        "pendiente" => gettext(locale, "Payment pending"),
        "parcial" => gettext(locale, "Partly paid"),
        "borrador" => gettext(locale, "Draft"),
        "cancelado" => gettext(locale, "Cancelled"),
        other => return other.to_owned(),
    }
    .into_owned()
}

/// An amount as people write it here: `45.000`, `45.000,50`, `$ 1,5`.
pub fn parse_amount(text: &str) -> Option<f64> {
    crate::leads::amount(text)
}

/// `90000.0` → `$ 90.000`; cents only when there are some.
pub fn money(amount: f64) -> String {
    let whole = amount.trunc() as i64;
    let cents = ((amount - amount.trunc()) * 100.0).round() as i64;
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push('.');
        }
        grouped.push(digit);
    }
    if cents > 0 {
        format!("$ {grouped},{cents:02}")
    } else {
        format!("$ {grouped}")
    }
}

/// Reads a lead's orders on a thread, into the store.
fn refresh(ctx: &egui::Context, config: &std::path::Path, lead: &Lead, slug: &str) {
    let Some(phone) = lead.phone.clone() else {
        return;
    };
    let (config, id, slug) = (config.to_owned(), lead.id.clone(), slug.to_owned());
    sales::start(ctx, &format!("orders:{id}"), move || {
        let found = sales::lookup(&slug, &phone)?;
        let mut store = Store::open(&config);
        // The project may have moved to another brand meanwhile.
        let same_brand = store
            .get(&id)
            .and_then(|lead| store.brands.get(&lead.project))
            .is_some_and(|brand| brand.slug == slug);
        if !same_brand {
            return Ok(Answer::Lookup(None));
        }
        if let Some(lead) = store.get_mut(&id) {
            lead.orders = Some(OrdersSeen {
                known: found.is_some(),
                orders: found.as_ref().map(|(_, o)| o.clone()).unwrap_or_default(),
                read_at: jiff::Timestamp::now().as_second(),
            });
            if let (Some((customer, _)), Some(linked)) = (&found, lead.eleva.as_mut())
                && linked.id == customer.id
            {
                linked.stage = customer.stage.clone();
                linked.phone = customer.phone.clone();
                linked.name = customer.name.clone();
            }
            lead.follow_orders();
        }
        store.save();
        Ok(Answer::Lookup(found))
    });
}

/// Whether a lead's orders are due to be read on their own: old enough,
/// not being read, and not failed a moment ago.
fn due(lead: &Lead) -> bool {
    let job = format!("orders:{}", lead.id);
    lead.phone.is_some()
        && lead
            .orders
            .as_ref()
            .is_none_or(|seen| jiff::Timestamp::now().as_second() - seen.read_at > STALE)
        && !sales::running(&job)
        && sales::failure(&job).is_none()
}

/// The project's customers sold on WhatsApp with no order in the brand, as
/// a line under the panel's sales numbers; its click picks the first. Reads
/// the orders of customers not read lately.
pub fn panel_line(
    app: &App,
    ui: &mut egui::Ui,
    leads: &Leads,
    project: &Project,
) -> Option<String> {
    let palette = app.palette;
    let locale = app.locale;
    let brand = leads.brands.get(&project.id)?;
    for lead in leads
        .of_project(&project.id)
        .filter(|lead| lead.stage == crate::leads::Stage::Customer && due(lead))
        .take(3)
    {
        refresh(ui.ctx(), &app.dirs.config, lead, &brand.slug);
    }
    let unregistered = leads.unregistered(&project.id);
    let first = unregistered.first()?;
    ui.add_space(6.0);
    let response = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            theme::icon(ui, Icon::CircleAlert, 14.0, palette.warning);
            theme::text(
                ui,
                gettext(locale, "Sold on WhatsApp, not in {brand}: {n}")
                    .replace("{brand}", &brand.name)
                    .replace("{n}", &unregistered.len().to_string()),
                theme::medium(13.0),
                palette.warning,
            );
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    response.clicked().then(|| first.id.clone())
}

/// The lead's orders in the brand, and registering a sale.
pub fn card(app: &App, ui: &mut egui::Ui, lead: &Lead, brand: &Brand) {
    let palette = app.palette;
    let locale = app.locale;
    let config = app.dirs.config.clone();
    let id = form_id(&lead.id);
    let mut form: Form = ui.ctx().data(|data| data.get_temp(id)).unwrap_or_default();
    demo_fill(&mut form, lead);
    let orders_job = format!("orders:{}", lead.id);
    let _ = sales::take(&orders_job);
    let reading = sales::running(&orders_job);
    if due(lead) {
        refresh(ui.ctx(), &config, lead, &brand.slug);
    }
    let orders_error = sales::failure(&orders_job);
    // What the brand's automations send, read once per brand.
    let automations_id = egui::Id::new(("eleva-automations", &brand.slug));
    let automations_job = format!("automations:{}", brand.slug);
    if let Some(Ok(Answer::Automations(found))) = sales::take(&automations_job) {
        ui.ctx()
            .data_mut(|data| data.insert_temp(automations_id, found));
    }
    let automations: Option<Automations> = ui.ctx().data(|data| data.get_temp(automations_id));
    let automations_error = sales::failure(&automations_job);
    if form.open
        && automations.is_none()
        && automations_error.is_none()
        && !sales::running(&automations_job)
    {
        let slug = brand.slug.clone();
        sales::start(ui.ctx(), &automations_job, move || {
            sales::automations(&slug).map(Answer::Automations)
        });
    }
    let key_job = format!("key:{}", brand.slug);
    let key_id = egui::Id::new(("eleva-api-key", &brand.slug));
    match sales::take(&key_job) {
        Some(Ok(_)) => {
            ui.ctx().data_mut(|data| data.insert_temp(key_id, true));
            form.error = None;
        }
        Some(Err(error)) => form.error = Some(error),
        None => {}
    }
    // Whether the brand's key is in the keyring, asked again while it is
    // not, so a key made from another lead counts here too.
    if form.open && form.has_key != Some(true) {
        let known = ui.ctx().data(|data| data.get_temp::<bool>(key_id)) == Some(true)
            || sales::has_key(&brand.slug);
        if known {
            ui.ctx().data_mut(|data| data.insert_temp(key_id, true));
        }
        form.has_key = Some(known);
    }
    let sale_job = format!("sale:{}", lead.id);
    match sales::take(&sale_job) {
        Some(Ok(Answer::Registered(registered))) if registered.created => {
            form = Form::default();
            sales::forget_failure(&orders_job);
            refresh(ui.ctx(), &config, lead, &brand.slug);
        }
        // The brand already had an order with this id: nothing new was made.
        Some(Ok(Answer::Registered(registered))) => {
            form.error = Some(format!("already:{}", registered.code));
        }
        Some(Err(error)) => form.error = Some(error),
        _ => {}
    }
    let sending = sales::running(&sale_job);
    card_frame(&palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            theme::icon(ui, Icon::Tag, 15.0, palette.accent);
            theme::text(
                ui,
                gettext(locale, "Orders in {brand}").replace("{brand}", &brand.name),
                theme::semibold(14.0),
                palette.text,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if reading {
                    ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                } else if lead.phone.is_some()
                    && theme::icon_button(
                        ui,
                        Icon::Refresh,
                        14.0,
                        palette.secondary,
                        palette.text,
                        &gettext(locale, "Read again"),
                    )
                    .clicked()
                {
                    sales::forget_failure(&orders_job);
                    refresh(ui.ctx(), &config, lead, &brand.slug);
                }
            });
        });
        ui.add_space(4.0);
        orders(app, ui, lead, brand, reading);
        if let Some(error) = &orders_error {
            wrapped(
                ui,
                &gettext(locale, "Their orders could not be read: {error}")
                    .replace("{error}", &error_text(locale, error)),
                theme::regular(12.0),
                palette.danger,
            );
        }
        for sale in &lead.sales {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
                theme::icon(ui, Icon::CheckCheck, 13.0, palette.accent);
                theme::text(
                    ui,
                    gettext(locale, "Registered from Whats Pro: {code} · {total}")
                        .replace("{code}", sale.code.as_deref().unwrap_or("—"))
                        .replace("{total}", &money(sale.total)),
                    theme::regular(12.5),
                    palette.secondary,
                );
                if let Some(notice) = &sale.notice {
                    chip(
                        ui,
                        &gettext(locale, "message: {state}").replace("{state}", &notice_text(locale, notice)),
                        palette.dim,
                    );
                }
            });
        }
        if lead.owes() {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                theme::icon(ui, Icon::CircleAlert, 14.0, palette.warning);
                theme::text(
                    ui,
                    gettext(locale, "Part of the payment is still due."),
                    theme::medium(13.0),
                    palette.warning,
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Only a sale put there from here can be marked paid here.
                    let ours = lead.sales.iter().any(|sale| sale.payment != "pagado");
                    if ours && button(ui, &palette, gettext(locale, "Mark as paid").as_ref(), false).clicked() {
                        let mut store = Store::open(&config);
                        if let Some(lead) = store.get_mut(&lead.id) {
                            lead.mark_paid();
                            lead.add(Kind::FollowUp, "✓ Cobrado por completo");
                        }
                        store.save();
                        drop(store);
                        crate::eleva::push(ui.ctx(), &config);
                    }
                });
            });
            if lead.orders.as_ref().is_some_and(|seen| {
                seen.orders.recent.iter().any(|o| matches!(o.payment.as_str(), "pendiente" | "parcial"))
            }) {
                wrapped(
                    ui,
                    gettext(locale, "An order in ElevaLeads is not paid: mark it paid there too, so the brand knows.").as_ref(),
                    theme::regular(12.0),
                    palette.dim,
                );
            }
        }
        if lead.needs_sale() && !form.open {
            ui.add_space(6.0);
            Frame::new()
                .fill(palette.warning.gamma_multiply(0.12))
                .corner_radius(egui::CornerRadius::same(theme::RADIUS + 2))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        theme::icon(ui, Icon::CircleAlert, 15.0, palette.warning);
                        ui.vertical(|ui| {
                            ui.set_max_width((ui.available_width() - 140.0).max(160.0));
                            theme::text(
                                ui,
                                gettext(locale, "Sold on WhatsApp, with no order in ElevaLeads")
                                    .as_ref(),
                                theme::semibold(13.0),
                                palette.text,
                            );
                            wrapped(
                                ui,
                                gettext(
                                    locale,
                                    "Register it so the brand counts the sale and the customer.",
                                )
                                .as_ref(),
                                theme::regular(12.0),
                                palette.secondary,
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(
                                ui,
                                &palette,
                                gettext(locale, "Register sale").as_ref(),
                                true,
                            )
                            .clicked()
                            {
                                open_form(&mut form, lead);
                            }
                        });
                    });
                });
        } else if !form.open && lead.phone.is_some() {
            ui.add_space(6.0);
            if button(
                ui,
                &palette,
                gettext(locale, "Register a sale").as_ref(),
                false,
            )
            .clicked()
            {
                open_form(&mut form, lead);
            }
        }
        if form.open {
            ui.add_space(8.0);
            sale_form(
                app,
                ui,
                &mut form,
                lead,
                brand,
                (automations.as_ref(), automations_error.as_deref()),
                sending,
            );
        }
        if let Some(error) = &form.error {
            ui.add_space(4.0);
            wrapped(
                ui,
                &error_text(locale, error),
                theme::regular(12.5),
                palette.danger,
            );
        }
    });
    ui.ctx().data_mut(|data| data.insert_temp(id, form));
}

fn open_form(form: &mut Form, lead: &Lead) {
    form.open = true;
    form.error = None;
    form.id = lead.next_sale_id();
    if form.items.is_empty() {
        form.items.push((String::new(), "1".into(), String::new()));
    }
    if form.total.is_empty()
        && let Some(value) = lead.value.as_deref().and_then(parse_amount)
    {
        form.total = money(value);
        form.total_typed = true;
    }
}

/// What the brand knows the customer bought.
fn orders(app: &App, ui: &mut egui::Ui, lead: &Lead, brand: &Brand, reading: bool) {
    let palette = app.palette;
    let locale = app.locale;
    let Some(seen) = &lead.orders else {
        let text = if lead.phone.is_none() {
            gettext(locale, "This lead has no number to look for.")
        } else if reading {
            gettext(locale, "Looking for their orders…")
        } else {
            gettext(locale, "Not read yet.")
        };
        theme::text(ui, text.as_ref(), theme::regular(13.0), palette.secondary);
        return;
    };
    if !seen.known {
        wrapped(
            ui,
            &gettext(
                locale,
                "{brand} has no customer with this number. Registering a sale creates them.",
            )
            .replace("{brand}", &brand.name),
            theme::regular(13.0),
            palette.secondary,
        );
        return;
    }
    if seen.orders.count == 0 {
        theme::text(
            ui,
            gettext(locale, "No orders in {brand}.").replace("{brand}", &brand.name),
            theme::regular(13.0),
            palette.secondary,
        );
    } else {
        theme::text(
            ui,
            if seen.orders.count == 1 {
                gettext(locale, "1 order · {spent}").replace("{spent}", &seen.orders.spent)
            } else {
                gettext(locale, "{n} orders · {spent}")
                    .replace("{n}", &seen.orders.count.to_string())
                    .replace("{spent}", &seen.orders.spent)
            },
            theme::semibold(13.5),
            palette.text,
        );
    }
    for order in seen.orders.recent.iter().take(3) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            chip(ui, &order.code, palette.secondary);
            theme::text(
                ui,
                status_label(locale, &order.status),
                theme::regular(12.5),
                palette.text,
            );
            let paid = order.payment == "pagado";
            theme::text(
                ui,
                status_label(locale, &order.payment),
                theme::regular(12.5),
                if paid {
                    palette.accent
                } else {
                    palette.warning
                },
            );
            theme::text(ui, &order.total, theme::regular(12.5), palette.secondary);
            theme::text(ui, &order.date, theme::regular(12.0), palette.dim);
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn sale_form(
    app: &App,
    ui: &mut egui::Ui,
    form: &mut Form,
    lead: &Lead,
    brand: &Brand,
    (automations, automations_error): (Option<&Automations>, Option<&str>),
    sending: bool,
) {
    let palette = app.palette;
    let locale = app.locale;
    theme::text(
        ui,
        gettext(locale, "What they bought").as_ref(),
        theme::medium(12.5),
        palette.secondary,
    );
    ui.add_space(2.0);
    let mut remove = None;
    let rows = form.items.len();
    for (index, (description, qty, price)) in form.items.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let width = ui.available_width();
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(description)
                        .id(egui::Id::new(("sale-item", &lead.id, index)))
                        .hint_text(gettext(locale, "Black leather belt").as_ref())
                        .font(theme::regular(13.5))
                        .frame(Frame::NONE)
                        .desired_width((width - 230.0).max(120.0)),
                );
            });
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(qty)
                        .id(egui::Id::new(("sale-qty", &lead.id, index)))
                        .hint_text("1")
                        .font(theme::regular(13.5))
                        .frame(Frame::NONE)
                        .desired_width(34.0),
                );
            });
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(price)
                        .id(egui::Id::new(("sale-price", &lead.id, index)))
                        .hint_text(gettext(locale, "price").as_ref())
                        .font(theme::regular(13.5))
                        .frame(Frame::NONE)
                        .desired_width(80.0),
                );
            });
            if rows > 1
                && theme::icon_button(
                    ui,
                    Icon::X,
                    13.0,
                    palette.dim,
                    palette.text,
                    &gettext(locale, "Remove"),
                )
                .clicked()
            {
                remove = Some(index);
            }
        });
        ui.add_space(3.0);
    }
    if let Some(index) = remove {
        form.items.remove(index);
    }
    if theme::icon_button(
        ui,
        Icon::Plus,
        15.0,
        palette.accent,
        palette.accent_hover,
        &gettext(locale, "Add an item"),
    )
    .clicked()
    {
        form.items.push((String::new(), "1".into(), String::new()));
    }
    let items: Vec<(String, f64, f64)> = form
        .items
        .iter()
        .filter(|(description, _, _)| !description.trim().is_empty())
        .map(|(description, qty, price)| {
            (
                description.trim().to_owned(),
                parse_amount(qty).filter(|q| *q > 0.0).unwrap_or(1.0),
                parse_amount(price).unwrap_or(0.0),
            )
        })
        .collect();
    let sum: f64 = items.iter().map(|(_, qty, price)| qty * price).sum();
    if !form.total_typed {
        form.total = if sum > 0.0 { money(sum) } else { String::new() };
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        theme::text(
            ui,
            gettext(locale, "Total").as_ref(),
            theme::medium(12.5),
            palette.secondary,
        );
        let response = field_frame(&palette)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut form.total)
                        .id(egui::Id::new(("sale-total", &lead.id)))
                        .hint_text("$ 0")
                        .font(theme::semibold(14.0))
                        .frame(Frame::NONE)
                        .desired_width(120.0),
                )
            })
            .inner;
        if response.changed() {
            form.total_typed = !form.total.trim().is_empty();
        }
    });
    ui.add_space(6.0);
    let pick = |ui: &mut egui::Ui, label: &str, options: &[&str], current: &mut usize| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
            theme::text(ui, label, theme::medium(12.5), palette.secondary);
            ui.add_space(4.0);
            for (index, option) in options.iter().enumerate() {
                if widgets::filter_chip(
                    ui,
                    &palette,
                    &status_label(locale, option),
                    0,
                    *current == index,
                )
                .clicked()
                {
                    *current = index;
                }
            }
        });
    };
    pick(
        ui,
        gettext(locale, "Order").as_ref(),
        &STATUSES,
        &mut form.status,
    );
    pick(
        ui,
        gettext(locale, "Payment").as_ref(),
        &PAYMENTS,
        &mut form.payment,
    );
    ui.add_space(6.0);
    field_frame(&palette).show(ui, |ui| {
        ui.add(
            egui::TextEdit::singleline(&mut form.notes)
                .id(egui::Id::new(("sale-notes", &lead.id)))
                .hint_text(
                    gettext(locale, "Note for the order (delivery, how they paid…)").as_ref(),
                )
                .font(theme::regular(13.5))
                .frame(Frame::NONE)
                .desired_width(f32::INFINITY),
        );
    });
    // What the customer will get, said before anything is sent.
    let status = STATUSES[form.status.min(STATUSES.len() - 1)];
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
        match automations {
            None if automations_error.is_some() => {
                theme::icon(ui, Icon::CircleAlert, 13.0, palette.danger);
                wrapped(
                    ui,
                    gettext(locale, "The brand's automatic messages could not be checked: the customer may get a WhatsApp message.").as_ref(),
                    theme::regular(12.5),
                    palette.danger,
                );
                let job = format!("automations:{}", brand.slug);
                if button(ui, &palette, gettext(locale, "Check again").as_ref(), false).clicked() {
                    sales::forget_failure(&job);
                }
                ui.checkbox(&mut form.unknown_ok, gettext(locale, "Register anyway"));
            }
            None => {
                ui.add(egui::Spinner::new().size(12.0).color(palette.dim));
                theme::text(ui, gettext(locale, "Checking the brand's automatic messages…").as_ref(), theme::regular(12.0), palette.dim);
            }
            Some(automations) => match automations.message(status) {
                Some(message) => {
                    theme::icon(ui, Icon::Send, 13.0, palette.warning);
                    wrapped(
                        ui,
                        &gettext(locale, "With “{status}”, ElevaLeads sends the customer on WhatsApp: “{message}”")
                            .replace("{status}", &status_label(locale, status))
                            .replace("{message}", message),
                        theme::regular(12.5),
                        palette.warning,
                    );
                }
                None => {
                    theme::icon(ui, Icon::BellOff, 13.0, palette.dim);
                    theme::text(
                        ui,
                        gettext(locale, "With “{status}”, no message goes to the customer.")
                            .replace("{status}", &status_label(locale, status)),
                        theme::regular(12.5),
                        palette.secondary,
                    );
                }
            },
        }
    });
    // Delivered also starts the after-sale message, days later.
    if status == "recepcion"
        && let Some((days, message, sends)) = automations.and_then(|a| a.after_sale.as_ref())
    {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
            theme::icon(ui, Icon::Clock, 13.0, palette.warning);
            let text = if *sends {
                gettext(locale, "And {n} days later: “{message}”")
            } else {
                gettext(locale, "And {n} days later it tries: “{message}” (it will not go out without an approved template)")
            };
            wrapped(
                ui,
                &text.replace("{n}", &days.to_string()).replace("{message}", message),
                theme::regular(12.5),
                palette.warning,
            );
        });
    }
    // The brand's API key, made from here or pasted.
    let has_key = *form
        .has_key
        .get_or_insert_with(|| sales::has_key(&brand.slug));
    let key_job = format!("key:{}", brand.slug);
    if !has_key {
        ui.add_space(8.0);
        wrapped(
            ui,
            gettext(
                locale,
                "Registering needs an API key of the brand, kept in this computer's keyring.",
            )
            .as_ref(),
            theme::regular(12.5),
            palette.secondary,
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            if sales::running(&key_job) {
                ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
            } else if brand.write
                && button(
                    ui,
                    &palette,
                    gettext(locale, "Create a key for Whats Pro").as_ref(),
                    true,
                )
                .clicked()
            {
                let slug = brand.slug.clone();
                sales::start(ui.ctx(), &key_job, move || {
                    sales::create_key(&slug).map(|()| Answer::Key)
                });
            }
            field_frame(&palette).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut form.key)
                        .id(egui::Id::new(("sale-key", &brand.slug)))
                        .hint_text(gettext(locale, "or paste one: eleva_…").as_ref())
                        .password(true)
                        .font(theme::regular(13.0))
                        .frame(Frame::NONE)
                        .desired_width(200.0),
                );
            });
            if !form.key.trim().is_empty()
                && button(ui, &palette, gettext(locale, "Save").as_ref(), false).clicked()
            {
                match sales::set_key(&brand.slug, &form.key) {
                    Ok(()) => {
                        form.key.clear();
                        form.has_key = Some(true);
                        form.error = None;
                    }
                    Err(error) => form.error = Some(error.to_string()),
                }
            }
        });
    }
    ui.add_space(8.0);
    let total = parse_amount(&form.total).unwrap_or(sum);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if sending {
                ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
                theme::text(
                    ui,
                    gettext(locale, "Registering…").as_ref(),
                    theme::regular(12.5),
                    palette.secondary,
                );
            } else {
                let checked = automations.is_some() || form.unknown_ok;
                let ready = has_key && checked && (total > 0.0 || !items.is_empty());
                let response = ui.add_enabled_ui(ready, |ui| {
                    button(
                        ui,
                        &palette,
                        gettext(locale, "Register sale").as_ref(),
                        true,
                    )
                });
                if response.inner.clicked() && ready {
                    form.error = None;
                    let sale = NewSale {
                        external_id: form.id.clone(),
                        name: lead.name.clone(),
                        phone: lead.phone.clone().unwrap_or_default(),
                        items: items.clone(),
                        total,
                        status: status.to_owned(),
                        payment: PAYMENTS[form.payment.min(PAYMENTS.len() - 1)].to_owned(),
                        notes: form.notes.clone(),
                    };
                    register(ui.ctx(), &app.dirs.config, lead, brand, sale);
                }
                if button(ui, &palette, gettext(locale, "Cancel").as_ref(), false).clicked() {
                    form.open = false;
                    form.error = None;
                }
            }
        });
    });
}

/// Sends the sale on a thread; on success the lead keeps it, notes it, and
/// is linked to the customer the brand answered with.
fn register(
    ctx: &egui::Context,
    config: &std::path::Path,
    lead: &Lead,
    brand: &Brand,
    sale: NewSale,
) {
    let (config, id, slug) = (config.to_owned(), lead.id.clone(), brand.slug.clone());
    sales::start(ctx, &format!("sale:{id}"), move || {
        let registered = sales::register(&slug, &sale)?;
        if !registered.created {
            return Ok(Answer::Registered(registered));
        }
        let mut store = Store::open(&config);
        if let Some(lead) = store.get_mut(&id) {
            if lead.stage != crate::leads::Stage::Customer {
                lead.set_stage(crate::leads::Stage::Customer, None);
            }
            let summary = sale
                .items
                .iter()
                .map(|(description, qty, _)| format!("{qty} × {description}"))
                .collect::<Vec<_>>()
                .join(", ");
            lead.sales.push(Sale {
                id: sale.external_id.clone(),
                at: jiff::Timestamp::now().as_second(),
                total: sale.total,
                summary: summary.clone(),
                code: (!registered.code.is_empty()).then(|| registered.code.clone()),
                notice: registered.notice.clone(),
                payment: sale.payment.clone(),
                settled: false,
            });
            let line = format!(
                "Venta registrada en ElevaLeads: {} · {}{}",
                if registered.code.is_empty() {
                    "—"
                } else {
                    &registered.code
                },
                money(sale.total),
                if summary.is_empty() {
                    String::new()
                } else {
                    format!(" · {summary}")
                }
            );
            lead.add(Kind::FollowUp, &line);
            // The order is there already: this line is not sent again as a note.
            if let Some(entry) = lead.entries.last_mut() {
                entry.sync = Sync::Sent;
            }
            if lead.eleva.is_none() && !registered.customer.is_empty() {
                lead.link(ElevaCustomer {
                    id: registered.customer.clone(),
                    // The brand's own name comes with the next read of the orders.
                    name: String::new(),
                    stage: None,
                    phone: None,
                });
            }
        }
        store.save();
        drop(store);
        Ok(Answer::Registered(registered))
    });
    ctx.request_repaint_after(Duration::from_millis(200));
}

/// A lead whose form opens filled in when it is drawn, for the demo.
static DEMO_FORM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Opens `lead`'s sale form, filled in, the next time its card is drawn.
pub fn demo_open(lead: &str) {
    *DEMO_FORM.lock().unwrap_or_else(|p| p.into_inner()) = Some(lead.to_owned());
}

fn demo_fill(form: &mut Form, lead: &Lead) {
    let mut guard = DEMO_FORM.lock().unwrap_or_else(|p| p.into_inner());
    if guard.as_deref() != Some(lead.id.as_str()) {
        return;
    }
    *guard = None;
    open_form(form, lead);
    form.items = vec![
        (
            "Cinturón de cuero negro".into(),
            "2".into(),
            "45.000".into(),
        ),
        ("Hebilla plateada".into(), "1".into(), "12.000".into()),
    ];
    form.has_key = Some(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_the_local_way() {
        assert_eq!(parse_amount("45.000"), Some(45_000.0));
        assert_eq!(parse_amount("$ 1.250.000"), Some(1_250_000.0));
        assert_eq!(parse_amount("45.000,50"), Some(45_000.5));
        assert_eq!(parse_amount("1,5"), Some(1.5));
        assert_eq!(parse_amount("2"), Some(2.0));
        assert_eq!(parse_amount("12.5"), Some(12.5));
        assert_eq!(parse_amount("abc"), None);
        assert_eq!(money(90_000.0), "$ 90.000");
        assert_eq!(money(1_250_000.5), "$ 1.250.000,50");
        assert_eq!(money(950.0), "$ 950");
    }
}
