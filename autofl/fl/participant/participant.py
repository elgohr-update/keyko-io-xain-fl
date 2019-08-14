from typing import Dict, List, Tuple

import numpy as np
import tensorflow as tf
from absl import logging

from autofl.datasets import prep
from autofl.types import KerasWeights

from . import ModelProvider

NUM_CLASSES = 10  # FIXME remove
BATCH_SIZE = 64  # FIXME remove


class Participant:
    # pylint: disable-msg=too-many-arguments
    def __init__(
        self,
        cid: str,
        model_provider: ModelProvider,
        xy_train: Tuple[np.ndarray, np.ndarray],
        xy_val: Tuple[np.ndarray, np.ndarray],
        num_classes: int = NUM_CLASSES,
        batch_size: int = BATCH_SIZE,
    ) -> None:
        assert xy_train[0].shape[0] == xy_train[1].shape[0]
        assert xy_val[0].shape[0] == xy_val[1].shape[0]
        self.cid = cid
        self.model_provider = model_provider
        self.num_classes: int = num_classes
        self.batch_size: int = batch_size
        # Training set
        self.xy_train = xy_train
        self.steps_train: int = int(xy_train[0].shape[0] / batch_size)
        # Validation set
        self.xy_val = xy_val
        self.steps_val: int = 1

    def train_round(self, theta: KerasWeights, epochs) -> KerasWeights:
        logging.info("Participant {}: train_round START".format(self.cid))
        model = self.model_provider.init_model()
        model.set_weights(theta)
        _ = self._train(model, epochs)
        theta_prime = model.get_weights()
        logging.info("Participant {}: train_round FINISH".format(self.cid))
        return theta_prime

    def _train(self, model, epochs: int) -> Dict[str, List[float]]:
        ds_train = prep.init_ds_train(self.xy_train, self.num_classes, self.batch_size)
        ds_val = prep.init_ds_val(self.xy_val, self.num_classes)

        hist = model.fit(
            ds_train,
            epochs=epochs,
            validation_data=ds_val,
            callbacks=[LoggingCallback(self.cid, logging.info)],
            shuffle=False,  # Shuffling is handled via tf.data.Dataset
            steps_per_epoch=self.steps_train,
            validation_steps=self.steps_val,
            verbose=0,
        )
        return cast_to_float(hist.history)

    def evaluate(
        self, theta: KerasWeights, xy_test: Tuple[np.ndarray, np.ndarray]
    ) -> Tuple[float, float]:
        model = self.model_provider.init_model()
        model.set_weights(theta)
        ds_val = prep.init_ds_val(xy_test)
        # Assume the validation `tf.data.Dataset` to yield exactly one batch containing
        # all examples in the validation set
        loss, accuracy = model.evaluate(ds_val, steps=1, verbose=0)
        return loss, accuracy


def cast_to_float(hist):
    for key in hist:
        for index, number in enumerate(hist[key]):
            hist[key][index] = float(number)
    return hist


class LoggingCallback(tf.keras.callbacks.Callback):
    def __init__(self, cid: str, print_fn):
        tf.keras.callbacks.Callback.__init__(self)
        self.cid = cid
        self.print_fn = print_fn

    def on_epoch_end(self, epoch, logs={}):
        msg = "CID {} epoch {}".format(self.cid, epoch)
        self.print_fn(msg)
