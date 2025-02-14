use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use sea_orm::{
    prelude::Decimal, sea_query::Table, sqlx::types::chrono::Local, ActiveModelTrait, ActiveValue,
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Schema,
    TransactionTrait,
};
use serde::Deserialize;
use tracing::error;

use crate::{backup::backup_db, scheduler, UsrState};

mod order;
mod order_status;

#[derive(Deserialize)]
pub struct PendingOrder {
    pub name: String,
    pub count: u32,
    pub unit_cost: Decimal,
    pub store_in: String,
    pub team: scheduler::Team,
    pub reason: String,
    pub vendor: String,
    pub link: String,
}

#[axum::debug_handler]
async fn new_order(
    State(state): State<&'static UsrState>,
    Json(pending_order): Json<PendingOrder>,
) -> (StatusCode, &'static str) {
    let webhook_msg = format!(
        "**New Order!**\n**Name:** {}\n**Vendor:** {}\n**Link:** {}\n**Count:** {}\n**Unit Cost:** ${}\n**Subtotal:** ${}\n**Team:** {}\n**Reason:** {}",
        pending_order.name,
        pending_order.vendor,
        pending_order.link,
        pending_order.count,
        pending_order.unit_cost,
        Decimal::from(pending_order.count) * pending_order.unit_cost,
        pending_order.team,
        pending_order.reason
    );
    let active_model = order::ActiveModel {
        id: ActiveValue::NotSet,
        name: ActiveValue::Set(pending_order.name),
        count: ActiveValue::Set(pending_order.count),
        unit_cost: ActiveValue::Set(pending_order.unit_cost),
        store_in: ActiveValue::Set(pending_order.store_in),
        team: ActiveValue::Set(pending_order.team),
        reason: ActiveValue::Set(pending_order.reason),
        vendor: ActiveValue::Set(pending_order.vendor),
        link: ActiveValue::Set(pending_order.link),
        ref_number: ActiveValue::NotSet,
    };
    let result = state
        .db
        .transaction(|tx| {
            Box::pin(async move {
                let model = active_model.insert(tx).await?;

                let active_model = order_status::ActiveModel {
                    order_id: ActiveValue::Set(model.id),
                    instance_id: ActiveValue::NotSet,
                    date: ActiveValue::Set(Local::now().naive_local()),
                    status: ActiveValue::Set(order_status::Status::New),
                };

                active_model.insert(tx).await?;

                Result::<_, sea_orm::DbErr>::Ok(model)
            })
        })
        .await;

    match result {
        Ok(m) => {
            backup_db(state);
            state
                .new_orders_webhook
                .as_ref()
                .map(|x| x.enqueue(m.id, webhook_msg));
            (StatusCode::OK, "")
        }
        Err(e) => {
            error!("Failed to create new order: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "")
        }
    }
}

#[derive(Deserialize)]
pub struct ChangeOrder {
    pub id: u32,
    pub name: String,
    pub count: u32,
    pub unit_cost: Decimal,
    pub store_in: String,
    pub team: scheduler::Team,
    pub reason: String,
    pub vendor: String,
    pub link: String,
}

#[axum::debug_handler]
async fn change_order(
    State(state): State<&'static UsrState>,
    Json(change_order): Json<ChangeOrder>,
) -> (StatusCode, &'static str) {
    match order_status::Entity::find()
        .filter(order_status::Column::OrderId.eq(change_order.id))
        .order_by_desc(order_status::Column::InstanceId)
        .one(&state.db)
        .await
    {
        Ok(Some(model)) => {
            if model.status != order_status::Status::New {
                return (StatusCode::BAD_REQUEST, "Order has already been processed");
            }
        }
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "Order not found");
        }
        Err(e) => {
            error!("Failed to find order: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "");
        }
    }
    let webhook_msg = format!(
        "***Order Changed***\n**Name:** {}\n**Vendor:** {}\n**Link:** {}\n**Count:** {}\n**Unit Cost:** ${}\n**Subtotal:** ${}\n**Team:** {}\n**Reason:** {}",
        change_order.name,
        change_order.vendor,
        change_order.link,
        change_order.count,
        change_order.unit_cost,
        Decimal::from(change_order.count) * change_order.unit_cost,
        change_order.team,
        change_order.reason
    );
    let active_model = order::ActiveModel {
        id: ActiveValue::Unchanged(change_order.id),
        name: ActiveValue::Set(change_order.name),
        count: ActiveValue::Set(change_order.count),
        unit_cost: ActiveValue::Set(change_order.unit_cost),
        store_in: ActiveValue::Set(change_order.store_in),
        team: ActiveValue::Set(change_order.team),
        reason: ActiveValue::Set(change_order.reason),
        vendor: ActiveValue::Set(change_order.vendor),
        link: ActiveValue::Set(change_order.link),
        ref_number: ActiveValue::NotSet,
    };
    if let Err(e) = active_model.update(&state.db).await {
        error!("Failed to change order: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "")
    } else {
        backup_db(state);
        state
            .new_orders_webhook
            .as_ref()
            .map(|x| x.enqueue(change_order.id, webhook_msg));
        (StatusCode::OK, "")
    }
}

#[derive(Deserialize)]
struct DeleteOrder {
    id: u32,
    #[serde(default)]
    force: bool,
}

#[axum::debug_handler]
async fn cancel_order(
    State(state): State<&'static UsrState>,
    Json(DeleteOrder { id, force }): Json<DeleteOrder>,
) -> (StatusCode, &'static str) {
    let webhook_msg;

    match order_status::Entity::find()
        .filter(order_status::Column::OrderId.eq(id))
        .order_by_desc(order_status::Column::InstanceId)
        .one(&state.db)
        .await
    {
        Ok(Some(model)) => {
            if !force && model.status != order_status::Status::New {
                return (StatusCode::BAD_REQUEST, "Order has already been processed");
            }
            let model = match order::Entity::find_by_id(id).one(&state.db).await {
                Ok(Some(model)) => model,
                Ok(None) => unreachable!(),
                Err(e) => {
                    error!("Failed to find order: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "");
                }
            };
            webhook_msg = format!(
                "***Order Cancelled***\n**Name:** {}\n**Count:** {}\n**Team:** {}",
                model.name, model.count, model.team,
            );
        }
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "Order not found");
        }
        Err(e) => {
            error!("Failed to find order: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "");
        }
    }

    if force {
        let result = state
            .db
            .transaction(|tx| {
                Box::pin(async move {
                    order::Entity::delete_by_id(id).exec(tx).await?;
                    order_status::Entity::delete_many()
                        .filter(order_status::Column::OrderId.eq(id))
                        .exec(tx)
                        .await?;
                    Result::<_, sea_orm::DbErr>::Ok(())
                })
            })
            .await;

        if let Err(e) = result {
            error!("Failed to force delete order: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "");
        }
    } else if let Err(e) = order::Entity::delete_by_id(id).exec(&state.db).await {
        error!("Failed to delete order: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, "");
    }

    state
        .new_orders_webhook
        .as_ref()
        .map(|x| x.enqueue(id, webhook_msg));
    backup_db(state);

    (StatusCode::OK, "")
}

#[derive(Deserialize)]
pub struct UpdateOrder {
    pub id: u32,
    pub status: order_status::Status,
    pub ref_number: Option<u32>,
}

#[axum::debug_handler]
async fn update_order(
    State(state): State<&'static UsrState>,
    Json(update_order): Json<UpdateOrder>,
) -> (StatusCode, &'static str) {
    let webhook_msg;
    let mut same_status = false;

    match order_status::Entity::find()
        .filter(order_status::Column::OrderId.eq(update_order.id))
        .order_by_desc(order_status::Column::InstanceId)
        .one(&state.db)
        .await
    {
        Ok(Some(model)) => {
            if model.status == order_status::Status::InStorage {
                return (StatusCode::BAD_REQUEST, "Order is already in storage");
            }
            if model.status == update_order.status {
                if update_order.ref_number.is_none() {
                    return (StatusCode::BAD_REQUEST, "Order is already in that state");
                }
                same_status = true;
            }
            let model = match order::Entity::find_by_id(update_order.id)
                .one(&state.db)
                .await
            {
                Ok(Some(model)) => model,
                Ok(None) => unreachable!(),
                Err(e) => {
                    error!("Failed to find order: {e}");
                    return (StatusCode::INTERNAL_SERVER_ERROR, "");
                }
            };
            if update_order.status == order_status::Status::InStorage {
                if model.store_in.is_empty() {
                    webhook_msg = format!(
                        "**Order Complete!**\n**Name:** {}\n**Team:** {}",
                        model.name, model.team
                    );
                } else {
                    webhook_msg = format!(
                        "**Order Complete!**\n**Name:** {}\n**Team:** {}\n**Location:** {}",
                        model.name, model.team, model.store_in
                    );
                }
            } else {
                webhook_msg = format!(
                    "**Order Update!**\n**Name:** {}\n**Team:** {}\n**Status:** {}",
                    model.name, model.team, update_order.status
                );
            }
        }
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "Order not found");
        }
        Err(e) => {
            error!("Failed to find order: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "");
        }
    }

    let result = state
        .db
        .transaction(|tx| {
            Box::pin(async move {
                if !same_status {
                    let active_model = order_status::ActiveModel {
                        order_id: ActiveValue::Set(update_order.id),
                        instance_id: ActiveValue::NotSet,
                        date: ActiveValue::Set(Local::now().naive_local()),
                        status: ActiveValue::Set(update_order.status),
                    };
    
                    active_model.insert(tx).await?;
                }

                let active_model = order::ActiveModel {
                    id: ActiveValue::Unchanged(update_order.id),
                    name: ActiveValue::NotSet,
                    count: ActiveValue::NotSet,
                    unit_cost: ActiveValue::NotSet,
                    store_in: ActiveValue::NotSet,
                    team: ActiveValue::NotSet,
                    reason: ActiveValue::NotSet,
                    vendor: ActiveValue::NotSet,
                    link: ActiveValue::NotSet,
                    ref_number: ActiveValue::Set(update_order.ref_number),
                };

                active_model.update(tx).await?;

                Result::<_, sea_orm::DbErr>::Ok(())
            })
        })
        .await;

    if let Err(e) = result {
        error!("Failed to update order status: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "")
    } else {
        if !same_status {
            state
                .order_updates_webhook
                .as_ref()
                .map(|x| x.enqueue(update_order.id, webhook_msg));
        }
        backup_db(state);
        (StatusCode::OK, "")
    }
}

#[axum::debug_handler]
async fn get_orders(State(state): State<&'static UsrState>) -> Response {
    let result = order::Entity::find().all(&state.db).await;

    match result {
        Ok(orders) => {
            let result = order_status::Entity::find().all(&state.db).await;

            match result {
                Ok(statuses) => Json(serde_json::json!({
                    "orders": orders,
                    "statuses": statuses
                }))
                .into_response(),
                Err(e) => {
                    error!("Failed to get orders: {e}");
                    (StatusCode::INTERNAL_SERVER_ERROR, "").into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to get orders: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "").into_response()
        }
    }
}

pub fn router() -> Router<&'static UsrState> {
    Router::new()
        .route("/new/order", post(new_order))
        .route("/change/order", post(change_order))
        .route("/del/order", delete(cancel_order))
        .route("/update/order", post(update_order))
        .route("/list/order", get(get_orders))
}

pub async fn reset_tables(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    let builder = db.get_database_backend();
    let schema = Schema::new(builder);

    db.execute(builder.build(Table::drop().table(order::Entity).if_exists()))
        .await?;
    db.execute(builder.build(&schema.create_table_from_entity(order::Entity)))
        .await?;
    db.execute(builder.build(Table::drop().table(order_status::Entity).if_exists()))
        .await?;
    db.execute(builder.build(&schema.create_table_from_entity(order_status::Entity)))
        .await?;

    Ok(())
}
