use crate::PerformCrud;
use actix_web::web::Data;
use lemmy_api_common::{
  comment::{CommentResponse, CreateComment},
  utils::{
    check_community_ban,
    check_community_deleted_or_removed,
    check_post_deleted_or_removed,
    get_local_user_view_from_jwt,
    get_post,
    local_site_to_slur_regex,
  },
  websocket::{
    send::{send_comment_ws_message, send_local_notifs},
    UserOperationCrud,
  },
  LemmyContext,
};
use lemmy_apub::{
  generate_local_apub_endpoint,
  objects::comment::ApubComment,
  protocol::activities::{create_or_update::note::CreateOrUpdateNote, CreateOrUpdateType},
  EndpointType,
};
use lemmy_db_schema::{
  source::{
    actor_language::CommunityLanguage,
    comment::{Comment, CommentInsertForm, CommentLike, CommentLikeForm, CommentUpdateForm},
    comment_reply::{CommentReply, CommentReplyUpdateForm},
    local_site::LocalSite,
    person_mention::{PersonMention, PersonMentionUpdateForm},
  },
  traits::{Crud, Likeable},
};
use lemmy_utils::{
  error::LemmyError,
  utils::{remove_slurs, scrape_text_for_mentions},
  ConnectionId,
};

#[async_trait::async_trait(?Send)]
impl PerformCrud for CreateComment {
  type Response = CommentResponse;

  #[tracing::instrument(skip(context, websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    websocket_id: Option<ConnectionId>,
  ) -> Result<CommentResponse, LemmyError> {
    let data: &CreateComment = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;
    let local_site = LocalSite::read(context.pool()).await?;

    let content_slurs_removed = remove_slurs(
      &data.content.clone(),
      &local_site_to_slur_regex(&local_site),
    );

    // Check for a community ban
    let post_id = data.post_id;
    let post = get_post(post_id, context.pool()).await?;
    let community_id = post.community_id;

    check_community_ban(local_user_view.person.id, community_id, context.pool()).await?;
    check_community_deleted_or_removed(community_id, context.pool()).await?;
    check_post_deleted_or_removed(&post)?;

    // Check if post is locked, no new comments
    if post.locked {
      return Err(LemmyError::from_message("locked"));
    }

    // Fetch the parent, if it exists
    let parent_opt = if let Some(parent_id) = data.parent_id {
      Comment::read(context.pool(), parent_id).await.ok()
    } else {
      None
    };

    // If there's a parent_id, check to make sure that comment is in that post
    // Strange issue where sometimes the post ID of the parent comment is incorrect
    if let Some(parent) = parent_opt.as_ref() {
      if parent.post_id != post_id {
        return Err(LemmyError::from_message("couldnt_create_comment"));
      }
    }

    // if no language is set, copy language from parent post/comment
    let parent_language = parent_opt
      .as_ref()
      .map(|p| p.language_id)
      .unwrap_or(post.language_id);
    let language_id = data.language_id.unwrap_or(parent_language);

    CommunityLanguage::is_allowed_community_language(
      context.pool(),
      Some(language_id),
      community_id,
    )
    .await?;

    let comment_form = CommentInsertForm::builder()
      .content(content_slurs_removed.clone())
      .post_id(data.post_id)
      .creator_id(local_user_view.person.id)
      .language_id(Some(language_id))
      .build();

    // Create the comment
    let comment_form2 = comment_form.clone();
    let parent_path = parent_opt.clone().map(|t| t.path);
    let inserted_comment = Comment::create(context.pool(), &comment_form2, parent_path.as_ref())
      .await
      .map_err(|e| LemmyError::from_error_message(e, "couldnt_create_comment"))?;

    // Necessary to update the ap_id
    let inserted_comment_id = inserted_comment.id;
    let protocol_and_hostname = context.settings().get_protocol_and_hostname();

    let apub_id = generate_local_apub_endpoint(
      EndpointType::Comment,
      &inserted_comment_id.to_string(),
      &protocol_and_hostname,
    )?;
    let updated_comment = Comment::update(
      context.pool(),
      inserted_comment_id,
      &CommentUpdateForm::builder().ap_id(Some(apub_id)).build(),
    )
    .await
    .map_err(|e| LemmyError::from_error_message(e, "couldnt_create_comment"))?;

    // Scan the comment for user mentions, add those rows
    let post_id = post.id;
    let mentions = scrape_text_for_mentions(&content_slurs_removed);
    let recipient_ids = send_local_notifs(
      mentions,
      &updated_comment,
      &local_user_view.person,
      &post,
      true,
      context,
    )
    .await?;

    // You like your own comment by default
    let like_form = CommentLikeForm {
      comment_id: inserted_comment.id,
      post_id,
      person_id: local_user_view.person.id,
      score: 1,
    };

    CommentLike::like(context.pool(), &like_form)
      .await
      .map_err(|e| LemmyError::from_error_message(e, "couldnt_like_comment"))?;

    let apub_comment: ApubComment = updated_comment.into();
    CreateOrUpdateNote::send(
      apub_comment.clone(),
      &local_user_view.person.clone().into(),
      CreateOrUpdateType::Create,
      context,
      &mut 0,
    )
    .await?;

    // If its a reply, mark the parent as read
    if let Some(parent) = parent_opt {
      let parent_id = parent.id;
      let comment_reply = CommentReply::read_by_comment(context.pool(), parent_id).await;
      if let Ok(reply) = comment_reply {
        CommentReply::update(
          context.pool(),
          reply.id,
          &CommentReplyUpdateForm { read: Some(true) },
        )
        .await
        .map_err(|e| LemmyError::from_error_message(e, "couldnt_update_replies"))?;
      }

      // If the parent has PersonMentions mark them as read too
      let person_id = local_user_view.person.id;
      let person_mention =
        PersonMention::read_by_comment_and_person(context.pool(), parent_id, person_id).await;
      if let Ok(mention) = person_mention {
        PersonMention::update(
          context.pool(),
          mention.id,
          &PersonMentionUpdateForm { read: Some(true) },
        )
        .await
        .map_err(|e| LemmyError::from_error_message(e, "couldnt_update_person_mentions"))?;
      }
    }

    send_comment_ws_message(
      inserted_comment.id,
      UserOperationCrud::CreateComment,
      websocket_id,
      data.form_id.clone(),
      Some(local_user_view.person.id),
      recipient_ids,
      context,
    )
    .await
  }
}
