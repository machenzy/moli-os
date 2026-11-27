# Onboarding : Google Cast (annonces)

1. Demander l'adresse IP de l'enceinte (fixe : elle sert d'identité). `_googlecast._tcp`
   répond aussi pour les Chromecast et les télés : ne proposer qu'une enceinte (TXT `md`).
2. `media_base` : Moli tel que l'enceinte le joint (`http://<IP de Moli>:<port>`), pas
   `localhost` : c'est l'enceinte qui va chercher le son.
3. Il faut un serveur Piper (Wyoming) : `tts = "hôte:port"`, `voice` facultatif (sinon la
   voix par défaut du serveur) ; `volume` = le niveau de départ des annonces (le point
   `announce_volume` ensuite).
4. Proposer `quiet_hours = true` si la maison a des `[guard.quiet_hours]` : rien n'est dit
   pendant ces heures, même par un automatisme.
   Pour un silence qui dépend d'autre chose que l'heure (plus long le week-end, les jours
   fériés…), `quiet_when = "<instance>:<appareil>/<clé>"` : rien n'est dit tant que ce point
   vaut `true` (une aide « Ne pas déranger » tenue par un automatisme).
5. Déclarer `kind = "cast"`, `host`, `media_base`. `moli-os check-config`, redémarrer.
6. Prévenir : ce que l'enceinte jouait s'arrête pendant une annonce.
7. Tester, humain dans la pièce : une phrase courte (`announce`), puis vérifier que le volume
   d'avant est revenu.
