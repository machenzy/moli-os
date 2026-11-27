# Google Cast (annonces)

- Cast v2 (`moli-net::cast`, le cadre de `androidtv`) : TLS sur 8009, trames protobuf autour
  de JSON. Le certificat TLS de l'enceinte change tout seul (chaque jour, à chaque
  redémarrage) : c'est son identité DeviceAuth qui est épinglée, et elle signe à chaque
  connexion le certificat TLS du moment (le mécanisme de supermomo31, #129). Vu sur un Home
  Mini : le certificat TLS épinglé un soir ne passait plus le lendemain matin. Annonces seulement, par `moli-announce`
  comme le Sonos et l'AirPlay : Piper en local (Wyoming), son publié par Moli, joué par le
  *Default Media Receiver* (`CC1AD845`).
- Une annonce : volume lu, volume de l'annonce réglé, appli lancée, `LOAD` du WAV, fin
  attendue (`IDLE` / `FINISHED`), appli arrêtée et volume d'avant remis, même après un échec.
- ⚠️ Ce que l'enceinte jouait s'arrête, comme avec Home Assistant.
- L'enceinte va chercher le son chez Moli : `media_base` doit être joignable depuis elle.
- Pas de fixture : les tests jouent des annonces entières contre une fausse enceinte.
- Reste : la musique, les groupes d'enceintes (port 32xxx), le nom lu sur l'enceinte.
