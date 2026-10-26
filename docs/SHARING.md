# Sharing

Share files and folders with other people, by invitation or with a public link, and use what
others share with you.

- [Invite people](#invite-people)
- [Create a public link](#create-a-public-link)
- [See what you share](#see-what-you-share)
- [Shared with you](#shared-with-you)

## Invite people

1. Select a file or folder and choose **Share…**.
2. Enter one or more email addresses and choose **Can view** or **Can edit**.
3. Send the invitation. People appear as members once they accept.

The admin role, which may also manage the share, is available from the command line
(`--role admin`).

From a terminal:

```bash
pdfs share Documents/Plans alex@example.com --role editor
pdfs members Documents/Plans          # members, pending invitations and the public link
```

`pdfs share-role` changes someone's role, and `pdfs unshare` removes one person or stops sharing
altogether. See the [command-line reference](CLI.md#sharing).

## Create a public link

In **Share…**, create a public link. Anyone with the link can open the item. You can protect the
link with a password and let it expire on a date. From a terminal, use `pdfs public-link create`
and `pdfs public-link remove`.

## See what you share

**Shared by me** lists everything you share. Filter it by links, by people, or by expired links.
Copy a link that is still valid, or stop several shares at once. `pdfs shared` prints the same
list.

## Shared with you

**Shared with me** lists what other people share with you, who shared it, and what you may do with
it. The same items are in the `Shared with me` folder inside `~/ProtonDrive`. Items where you are
a viewer are read-only.

- **Invitations** wait in a banner at the top of the page until you accept or reject them. From a
  terminal: `pdfs invitations list`, then `accept ID` or `reject ID`.
- **Saved public links** have their own view. `pdfs bookmarks` manages them.
- **Leave…** a share you no longer need, or run `pdfs leave UID` with the ID from
  `pdfs shared-with-me`.
